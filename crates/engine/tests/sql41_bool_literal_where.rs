//! `WHERE` での真偽リテラル `TRUE`／`FALSE` の受理（SQL-41。`docs/spec/04-behavior/`
//! のポインタのみ。Issue #1526）の結合テスト。`tests/sql_where_or.rs` と同じ流儀
//! （実 `Storage`＋`CpuScalarProvider`、`execute_sql`／`execute_sql_in_session`）で、
//! 検索・広域取得・集計・述語つき DML・RLS 境界・恒真 DML の fail-closed を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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
            ColumnDef::new("flag", ColumnType::Boolean, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql41-bool-literal");
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

fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str, flag: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, flag) VALUES ({id}, '[0.{id},0.1]', '{lang}', {flag}) \
             USING OPERATION_ID 'seed-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert id={id} should succeed: {e:?}"));
}

fn select_ids_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {predicate} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("scan with predicate={predicate:?} should succeed: {e:?}"));
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn distance_ids_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!(
                "SELECT id FROM {TABLE} WHERE {predicate} ORDER BY embedding <=> '[0.5,0.1]' LIMIT 100"
            ),
        )
        .unwrap_or_else(|e| panic!("distance search with predicate={predicate:?} should succeed: {e:?}"));
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn count_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> u64 {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE {predicate}"),
        )
        .unwrap_or_else(|e| panic!("count(*) with predicate={predicate:?} should succeed: {e:?}"));
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

/// 5 行のコーパス: 1=ja/true, 2=en/true, 3=fr/false, 4=en/false, 5=de/NULL。
fn seed_corpus(core: &EngineCore, ctx: &PolicyContext) {
    insert(core, ctx, 1, "ja", "true");
    insert(core, ctx, 2, "en", "true");
    insert(core, ctx, 3, "fr", "false");
    insert(core, ctx, 4, "en", "false");
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (5, '[0.5,0.1]', 'de') \
             USING OPERATION_ID 'seed-5'"
        ),
    )
    .expect("insert id=5 (flag NULL) should succeed");
}

fn dml_err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    match core.execute_sql_in_session(ctx, &mut SessionState::default(), sql) {
        Err(e) => format!("{e:?}"),
        Ok(o) => panic!("expected rejection for {sql:?}, got {o:?}"),
    }
}

#[test]
fn sql41_scan_true_false_and_combinations() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);
    let all = vec![1, 2, 3, 4, 5];
    let none = Vec::<u64>::new();
    assert_eq!(select_ids_where(&core, &ctx, "TRUE"), all);
    assert_eq!(select_ids_where(&core, &ctx, "false"), none);
    assert_eq!(
        select_ids_where(&core, &ctx, "lang = 'ja' AND TRUE"),
        vec![1]
    );
    assert_eq!(select_ids_where(&core, &ctx, "lang = 'ja' AND FALSE"), none);
    assert_eq!(
        select_ids_where(&core, &ctx, "lang = 'ja' OR FALSE"),
        vec![1]
    );
    assert_eq!(select_ids_where(&core, &ctx, "lang = 'ja' OR TRUE"), all);
    assert_eq!(select_ids_where(&core, &ctx, "TRUE OR lang = 'ja'"), all);
    assert_eq!(
        select_ids_where(&core, &ctx, "(lang = 'ja' OR TRUE) AND lang = 'en'"),
        vec![2, 4]
    );
    assert_eq!(select_ids_where(&core, &ctx, "(TRUE)"), all);
    assert_eq!(select_ids_where(&core, &ctx, "NOT TRUE"), none);
    assert_eq!(select_ids_where(&core, &ctx, "NOT FALSE"), all);
    assert_eq!(select_ids_where(&core, &ctx, "NOT NOT TRUE"), all);
    assert_eq!(select_ids_where(&core, &ctx, "NOT (TRUE)"), none);
    assert_eq!(
        select_ids_where(&core, &ctx, "NOT (lang = 'ja' OR TRUE)"),
        none
    );
    assert_eq!(
        select_ids_where(&core, &ctx, "NOT (lang = 'ja' AND FALSE)"),
        all
    );
}

#[test]
fn sql41_distance_and_count_follow_the_same_semantics() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);
    assert_eq!(distance_ids_where(&core, &ctx, "TRUE"), vec![1, 2, 3, 4, 5]);
    assert_eq!(distance_ids_where(&core, &ctx, "FALSE"), Vec::<u64>::new());
    assert_eq!(count_where(&core, &ctx, "TRUE"), 5);
    assert_eq!(count_where(&core, &ctx, "FALSE"), 0);
    assert_eq!(count_where(&core, &ctx, "lang = 'en' AND TRUE"), 2);
}

#[test]
fn sql41_existing_boolean_forms_are_unchanged() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);
    assert_eq!(select_ids_where(&core, &ctx, "flag"), vec![1, 2]);
    assert_eq!(select_ids_where(&core, &ctx, "flag = true"), vec![1, 2]);
    assert_eq!(select_ids_where(&core, &ctx, "flag = false"), vec![3, 4]);
}

#[test]
fn sql41_predicate_dml_false_affects_zero_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);
    match core
        .execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!("DELETE FROM {TABLE} WHERE FALSE USING OPERATION_ID 'op-del-false'"),
        )
        .expect("DELETE WHERE FALSE should succeed")
    {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected Delete, got {other:?}"),
    }
    match core
        .execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET lang = 'xx' WHERE FALSE USING OPERATION_ID 'op-upd-false'"
            ),
        )
        .expect("UPDATE WHERE FALSE should succeed")
    {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected Update, got {other:?}"),
    }
    assert_eq!(count_where(&core, &ctx, "TRUE"), 5);
    assert_eq!(count_where(&core, &ctx, "lang = 'xx'"), 0);
}

#[test]
fn sql41_predicate_dml_and_true_matches_plain_predicate() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);
    match core
        .execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!("DELETE FROM {TABLE} WHERE lang = 'en' AND TRUE USING OPERATION_ID 'op-del-and-true'"),
        )
        .expect("DELETE WHERE lang='en' AND TRUE should succeed")
    {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected Delete, got {other:?}"),
    }
    assert_eq!(select_ids_where(&core, &ctx, "TRUE"), vec![1, 3, 5]);
}

#[test]
fn sql41_trivially_true_predicate_dml_is_rejected_without_side_effects() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);
    for (i, pred) in [
        "TRUE",
        "visible() AND TRUE",
        "lang = 'ja' OR TRUE",
        "(TRUE OR FALSE)",
    ]
    .iter()
    .enumerate()
    {
        let e = dml_err(
            &core,
            &ctx,
            &format!("DELETE FROM {TABLE} WHERE {pred} USING OPERATION_ID 'op-d-{i}'"),
        );
        assert!(
            e.contains("42601") || e.contains("unconditional"),
            "{pred}: {e}"
        );
        let e = dml_err(
            &core,
            &ctx,
            &format!("UPDATE {TABLE} SET lang = 'xx' WHERE {pred} USING OPERATION_ID 'op-u-{i}'"),
        );
        assert!(
            e.contains("42601") || e.contains("unconditional"),
            "{pred}: {e}"
        );
    }
    assert_eq!(count_where(&core, &ctx, "TRUE"), 5);
    assert_eq!(count_where(&core, &ctx, "lang = 'xx'"), 0);
}

#[test]
fn sql41_rls_boundary_is_preserved() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("tenant-a");
    let bob = ctx_for("tenant-b");
    seed_corpus(&core, &alice);
    insert(&core, &bob, 100, "ja", "true");
    assert_eq!(select_ids_where(&core, &bob, "TRUE"), vec![100]);
    assert_eq!(
        select_ids_where(&core, &bob, "lang = 'zz' OR TRUE"),
        vec![100]
    );
    assert_eq!(count_where(&core, &bob, "TRUE"), 1);
    core.execute_sql_in_session(
        &bob,
        &mut SessionState::default(),
        &format!("DELETE FROM {TABLE} WHERE FALSE USING OPERATION_ID 'op-bob-false'"),
    )
    .expect("DELETE WHERE FALSE should succeed");
    assert_eq!(count_where(&core, &alice, "TRUE"), 5);
    assert_eq!(count_where(&core, &bob, "TRUE"), 1);
}

#[test]
fn sql41_explain_does_not_panic() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);
    for pred in ["TRUE", "FALSE"] {
        let _ = core.execute_sql(
            &ctx,
            &format!("EXPLAIN SELECT id FROM {TABLE} WHERE {pred} LIMIT 10"),
        );
    }
}

/// PR #2297 指摘: 引用付き `"true"`／`"false"` を列名として使う DDL・INSERT・投影・
/// WHERE の各経路が従来どおり通る（WHERE の裸参照は列参照のまま）。
#[test]
fn sql41_quoted_bool_words_remain_usable_identifiers() {
    let path = unique_db_path("sql41-quoted-bool-ident");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let run = |sql: &str| {
        core.execute_sql_in_session(&ctx, &mut SessionState::default(), sql)
            .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"))
    };
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut ddl,
        "CREATE TABLE qdocs (embedding VECTOR(2) NOT NULL, \"true\" BOOLEAN, \"false\" BOOLEAN)",
    )
    .expect("quoted bool-word column names are valid DDL identifiers");
    run(
        "INSERT INTO qdocs (id, embedding, \"true\", \"false\") VALUES (1, '[0.1,0.1]', true, false) \
         USING OPERATION_ID 'q-1'",
    );
    run(
        "INSERT INTO qdocs (id, embedding, \"true\", \"false\") VALUES (2, '[0.2,0.1]', false, true) \
         USING OPERATION_ID 'q-2'",
    );
    let ids = |sql: &str| -> Vec<u64> {
        let mut v: Vec<u64> = core
            .execute_sql(&ctx, sql)
            .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"))
            .rows
            .iter()
            .map(|r| r.id)
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(ids("SELECT \"true\" FROM qdocs LIMIT 10"), vec![1, 2]);
    assert_eq!(ids("SELECT id FROM qdocs WHERE \"true\" LIMIT 10"), vec![1]);
    assert_eq!(
        ids("SELECT id FROM qdocs WHERE \"false\" LIMIT 10"),
        vec![2]
    );
    assert_eq!(
        ids("SELECT id FROM qdocs WHERE \"true\" = true LIMIT 10"),
        vec![1]
    );
    assert_eq!(
        ids("SELECT id FROM qdocs WHERE NOT \"true\" LIMIT 10"),
        vec![2]
    );
    assert_eq!(
        ids("SELECT id FROM qdocs WHERE (\"true\") AND TRUE LIMIT 10"),
        vec![1]
    );
}

/// PR #2297 再指摘: JOIN の WHERE でも引用付き `"true"`／`"false"` の裸参照は列参照として
/// 受理される（片方のテーブルだけが当該列を持つ場合。従来有効だった経路の互換維持）。
#[test]
fn sql41_join_where_quoted_bool_words_remain_column_refs() {
    let path = unique_db_path("sql41-quoted-bool-join");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    for sql in [
        "CREATE TABLE ja (embedding VECTOR(2) NOT NULL, \"true\" BOOLEAN, \"false\" BOOLEAN)",
        "CREATE TABLE jb (embedding VECTOR(2) NOT NULL)",
    ] {
        core.execute_sql_in_session(&ctx, &mut ddl, sql)
            .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"));
    }
    let run = |sql: &str| {
        core.execute_sql_in_session(&ctx, &mut SessionState::default(), sql)
            .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"))
    };
    run("INSERT INTO ja (id, embedding, \"true\", \"false\") VALUES (1, '[0.1,0.1]', true, false) USING OPERATION_ID 'j-1'");
    run("INSERT INTO ja (id, embedding, \"true\", \"false\") VALUES (2, '[0.2,0.1]', false, true) USING OPERATION_ID 'j-2'");
    run("INSERT INTO jb (id, embedding) VALUES (1, '[0.1,0.1]') USING OPERATION_ID 'j-3'");
    run("INSERT INTO jb (id, embedding) VALUES (2, '[0.2,0.1]') USING OPERATION_ID 'j-4'");
    for pred in ["\"true\"", "\"false\""] {
        let sql = format!("SELECT ja.id FROM ja JOIN jb ON ja.id = jb.id WHERE {pred} LIMIT 10");
        let result = core
            .execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"));
        assert_eq!(result.rows.len(), 1, "{sql}");
    }
}
/// PR #2297 指摘: 括弧付きの引用付き `"true"` が比較の左辺（値式）になる場合も、
/// 従来どおり数値列の列参照として受理される。
#[test]
fn sql41_parenthesized_quoted_bool_word_value_expr_remains_column_ref() {
    let path = unique_db_path("sql41-quoted-bool-paren-value");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut ddl,
        "CREATE TABLE ndocs (embedding VECTOR(2) NOT NULL, \"true\" INTEGER)",
    )
    .expect("create table");
    for (id, v) in [(1, 5), (2, 0)] {
        core.execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO ndocs (id, embedding, \"true\") VALUES ({id}, '[0.{id},0.1]', {v}) \
                 USING OPERATION_ID 'n-{id}'"
            ),
        )
        .expect("insert");
    }
    let sql = "SELECT id FROM ndocs WHERE (\"true\") > 0 LIMIT 10";
    let result = core
        .execute_sql(&ctx, sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"));
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![1]);
}

/// PR #2297 指摘: 引用付き `"true"` 列の裸参照を含むビュー定義は、保存された正規化 SQL の
/// 再解析後も列述語のまま（`"true"` が真の行だけが残る）。
#[test]
fn sql41_view_roundtrip_keeps_quoted_bool_word_column_predicate() {
    let path = unique_db_path("sql41-quoted-bool-view");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    for sql in [
        "CREATE TABLE vdocs (embedding VECTOR(2) NOT NULL, \"true\" BOOLEAN)",
        "CREATE VIEW vv AS SELECT id FROM vdocs WHERE \"true\"",
    ] {
        core.execute_sql_in_session(&ctx, &mut ddl, sql)
            .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"));
    }
    for (id, v) in [(1, "true"), (2, "false")] {
        core.execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO vdocs (id, embedding, \"true\") VALUES ({id}, '[0.{id},0.1]', {v}) \
                 USING OPERATION_ID 'v-{id}'"
            ),
        )
        .expect("insert");
    }
    let result = core
        .execute_sql(&ctx, "SELECT id FROM vv LIMIT 10")
        .expect("view select");
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![1]);
}

/// PR #2297 指摘（互換性）: 旧版が列参照 `"true"` を未引用の `WHERE true` で永続化したビューは、
/// 現行版の参照時再解析でも恒真化せず列述語のまま（`"true"` が真の行だけが残る）。
/// 旧保存形式は公開 API の `Storage::create_view` で直接再現する。
#[test]
fn sql41_legacy_stored_view_body_keeps_bare_true_as_column_predicate() {
    let path = unique_db_path("sql41-legacy-bool-view");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage
        .create_table(&TableSchema::new(
            "vdocs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("true", ColumnType::Boolean, true),
            ],
        ))
        .expect("create table");
    storage
        .create_view("vv", "vdocs", "SELECT id FROM vdocs WHERE true")
        .expect("legacy view body");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    for (id, v) in [(1, "true"), (2, "false")] {
        core.execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO vdocs (id, embedding, \"true\") VALUES ({id}, '[0.{id},0.1]', {v}) \
                 USING OPERATION_ID 'v-{id}'"
            ),
        )
        .expect("insert");
    }
    let result = core
        .execute_sql(&ctx, "SELECT id FROM vv LIMIT 10")
        .expect("view select");
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![1]);
}

/// 列 `"true"` を持つ vdocs を作る（id=1 が真・id=2 が偽・id=3 が NULL を後で投入する）。
fn bool_column_storage(name: &str) -> (Storage, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "vdocs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("true", ColumnType::Boolean, true),
            ],
        ))
        .expect("create table");
    (storage, path)
}

fn seed_bool_column(core: &EngineCore, ctx: &PolicyContext) {
    for (id, v) in [(1, "true"), (2, "false"), (3, "NULL")] {
        core.execute_sql_in_session(
            ctx,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO vdocs (id, embedding, \"true\") VALUES ({id}, '[0.{id},0.1]', {v}) \
                 USING OPERATION_ID 'v-{id}'"
            ),
        )
        .expect("insert");
    }
}

fn ids_of(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"));
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

/// PR #2297 指摘: 旧版が保存した評価後射影形（`LIMIT` 付き）のビュー本文も列述語のまま。
#[test]
fn sql41_legacy_buffered_view_body_keeps_bare_true_as_column_predicate() {
    let (storage, path) = bool_column_storage("sql41-legacy-buffered-view");
    let _guard = CleanupGuard(path);
    storage
        .create_view("vv", "vdocs", "SELECT id FROM vdocs WHERE true LIMIT 10")
        .expect("legacy buffered body");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    seed_bool_column(&core, &ctx);
    assert_eq!(ids_of(&core, &ctx, "SELECT id FROM vv LIMIT 10"), vec![1]);
}

/// PR #2297 指摘: CTE 本文内の `WHERE "true"` は閉じ括弧の外側が `SELECT` でも列述語。
#[test]
fn sql41_cte_inner_quoted_bool_word_stays_column_predicate() {
    let (storage, path) = bool_column_storage("sql41-cte-quoted-bool");
    let _guard = CleanupGuard(path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    seed_bool_column(&core, &ctx);
    assert_eq!(
        ids_of(
            &core,
            &ctx,
            "WITH q AS (SELECT id FROM vdocs WHERE \"true\") SELECT id FROM q LIMIT 10"
        ),
        vec![1]
    );
}

/// PR #2297 指摘: `CREATE VIEW ... WHERE FALSE`／`TRUE` は真偽リテラルとして保存・再解析される
/// （旧列述語互換の昇格で列参照へ化けない）。
#[test]
fn sql41_view_with_bool_literal_roundtrips_as_literal() {
    let (storage, path) = bool_column_storage("sql41-view-literal");
    let _guard = CleanupGuard(path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    for sql in [
        "CREATE VIEW vf AS SELECT id FROM vdocs WHERE FALSE ORDER BY id LIMIT 10",
        "CREATE VIEW vt AS SELECT id FROM vdocs WHERE TRUE ORDER BY id LIMIT 10",
    ] {
        core.execute_sql_in_session(&ctx, &mut ddl, sql)
            .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"));
    }
    seed_bool_column(&core, &ctx);
    assert_eq!(
        ids_of(&core, &ctx, "SELECT id FROM vf LIMIT 10"),
        Vec::<u64>::new()
    );
    assert_eq!(
        ids_of(&core, &ctx, "SELECT id FROM vt LIMIT 10"),
        vec![1, 2, 3]
    );
}
