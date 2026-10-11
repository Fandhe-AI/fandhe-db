//! スキーマ定義系 3 上限（列数・配列要素数・ENUM ラベル数）の起動時設定
//! （Issue #1548。ポインタ: `docs/spec/04-behavior/` SQL-46）の結合テスト。
//!
//! `EngineCore::with_schema_limits` で設定値を注入し、`execute_sql_in_session`
//! （wire-server の simple／extended プロトコルと NoSQL 表層が共有する実行入口）を
//! production 経路として検証する。確認する契約:
//!
//! - 設定値ちょうどは成功し、超過は従来と同じ SQLSTATE（列数・ラベル数は `54000`、
//!   配列の `[N]` 宣言は `42601`）で拒否される
//! - `[]`（要素数省略）の配列は設定値を上限にする
//! - 上限を下げて開き直しても既存のテーブル・型は読み書きでき、定義が増える
//!   ADD COLUMN だけが拒否される（設定値を decode 経路へ入れない不変条件）
//! - 既定値はハード上限と同値で、未注入と同一に振る舞う

use engine::catalog::{ColumnType, MAX_ARRAY_ELEMENTS, MAX_COLUMN_COUNT, MAX_ENUM_LABELS};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::schema_limits::SchemaLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn core_at(path: &std::path::Path, limits: SchemaLimits) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_schema_limits(limits)
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), s, sql)
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> &'static str {
    match run(core, s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// 埋め込み列 1 つ＋テキスト列でちょうど `n` 列の CREATE TABLE 文。
fn create_table_sql(name: &str, n: usize) -> String {
    let mut cols = vec!["embedding VECTOR(2)".to_string()];
    cols.extend((1..n).map(|i| format!("c{i} TEXT")));
    format!("CREATE TABLE {name} ({})", cols.join(", "))
}

fn limits(cols: usize, arr: u32, labels: usize) -> SchemaLimits {
    SchemaLimits::new(cols, arr, labels)
}

#[test]
fn default_limits_equal_hard_limits_and_match_uninjected_core() {
    let d = SchemaLimits::default();
    assert_eq!(d.max_table_columns(), MAX_COLUMN_COUNT);
    assert_eq!(d.max_array_elements(), MAX_ARRAY_ELEMENTS);
    assert_eq!(d.max_enum_labels(), MAX_ENUM_LABELS);

    let path = unique_db_path("schema-limits-default");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(core.schema_limits(), SchemaLimits::default());
    let mut s = session();
    run(&core, &mut s, &create_table_sql("t", MAX_COLUMN_COUNT)).expect("hard limit ok");
    assert_eq!(
        code(&core, &mut s, &create_table_sql("u", MAX_COLUMN_COUNT + 1)),
        "54000"
    );
}

#[test]
fn create_table_column_count_boundary_follows_setting() {
    for n in [1usize, 4, 100, MAX_COLUMN_COUNT] {
        let path = unique_db_path("schema-limits-cols");
        let _g = CleanupGuard(path.clone());
        let core = core_at(&path, limits(n, MAX_ARRAY_ELEMENTS, MAX_ENUM_LABELS));
        let mut s = session();
        run(&core, &mut s, &create_table_sql("ok_t", n))
            .unwrap_or_else(|e| panic!("{n} columns must succeed: {e}"));
        assert_eq!(
            code(&core, &mut s, &create_table_sql("ng_t", n + 1)),
            "54000",
            "limit {n}"
        );
    }
}

#[test]
fn add_column_boundary_follows_setting_and_counts_tombstones() {
    let path = unique_db_path("schema-limits-add-col");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path, limits(4, MAX_ARRAY_ELEMENTS, MAX_ENUM_LABELS));
    let mut s = session();
    run(&core, &mut s, &create_table_sql("t", 2)).expect("create");
    run(&core, &mut s, "ALTER TABLE t ADD COLUMN a TEXT").expect("3rd column");
    run(&core, &mut s, "ALTER TABLE t ADD COLUMN b TEXT").expect("4th column");
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE t ADD COLUMN c TEXT"),
        "54000"
    );
    // 墓標も物理スロットとして数える（DROP COLUMN 後も ADD は拒否される）。
    run(&core, &mut s, "ALTER TABLE t DROP COLUMN a").expect("drop");
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE t ADD COLUMN c TEXT"),
        "54000"
    );
}

#[test]
fn array_declaration_boundary_follows_setting() {
    for m in [8u32, MAX_ARRAY_ELEMENTS] {
        let path = unique_db_path("schema-limits-array");
        let _g = CleanupGuard(path.clone());
        let core = core_at(&path, limits(MAX_COLUMN_COUNT, m, MAX_ENUM_LABELS));
        let mut s = session();
        run(
            &core,
            &mut s,
            &format!("CREATE TABLE a (embedding VECTOR(2), x INTEGER[{m}])"),
        )
        .expect("exact limit");
        assert_eq!(
            code(
                &core,
                &mut s,
                &format!("CREATE TABLE b (embedding VECTOR(2), x INTEGER[{}])", m + 1)
            ),
            "42601"
        );
        run(
            &core,
            &mut s,
            &format!("ALTER TABLE a ADD COLUMN y INTEGER[{m}]"),
        )
        .expect("add exact");
        assert_eq!(
            code(
                &core,
                &mut s,
                &format!("ALTER TABLE a ADD COLUMN z INTEGER[{}]", m + 1)
            ),
            "42601"
        );
        assert_eq!(
            code(
                &core,
                &mut s,
                &format!("ALTER TABLE a ALTER COLUMN y TYPE BIGINT[{}]", m + 1)
            ),
            "42601"
        );
    }
}

#[test]
fn array_without_length_uses_setting_as_max_len() {
    let path = unique_db_path("schema-limits-array-default");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path, limits(MAX_COLUMN_COUNT, 3, MAX_ENUM_LABELS));
    let mut s = session();
    run(
        &core,
        &mut s,
        "CREATE TABLE t (embedding VECTOR(2), n INTEGER[])",
    )
    .expect("create with [] must not be rejected by its own default");
    run(&core, &mut s, "ALTER TABLE t ADD COLUMN m TEXT[]").expect("add []");
    run(
        &core,
        &mut s,
        "INSERT INTO t (id, embedding, n) VALUES (1, '[0.1,0.2]', '{1,2,3}') \
         USING OPERATION_ID 'op-1'",
    )
    .expect("exactly max_len elements");
    assert_eq!(
        code(
            &core,
            &mut s,
            "INSERT INTO t (id, embedding, n) VALUES (2, '[0.1,0.2]', '{1,2,3,4}') \
             USING OPERATION_ID 'op-2'"
        ),
        "54000"
    );
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("t").expect("schema");
    for col in schema.columns.iter().filter(|c| c.name != "embedding") {
        match &col.ty {
            ColumnType::Array(a) => assert_eq!(a.max_len(), 3, "{}", col.name),
            other => panic!("unexpected type {other:?}"),
        }
    }
}

#[test]
fn enum_label_boundary_follows_setting() {
    for l in [2usize, MAX_ENUM_LABELS] {
        let path = unique_db_path("schema-limits-enum");
        let _g = CleanupGuard(path.clone());
        let core = core_at(&path, limits(MAX_COLUMN_COUNT, MAX_ARRAY_ELEMENTS, l));
        let mut s = session();
        let labels = |n: usize| {
            (0..n)
                .map(|i| format!("'l{i}'"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        run(
            &core,
            &mut s,
            &format!("CREATE TYPE ok_e AS ENUM ({})", labels(l)),
        )
        .expect("exact limit");
        assert_eq!(
            code(
                &core,
                &mut s,
                &format!("CREATE TYPE ng_e AS ENUM ({})", labels(l + 1))
            ),
            "54000",
            "limit {l}"
        );
    }
}

#[test]
fn lowering_limits_keeps_existing_schema_readable_and_writable() {
    let path = unique_db_path("schema-limits-persisted");
    let _g = CleanupGuard(path.clone());
    {
        let core = core_at(&path, SchemaLimits::default());
        let mut s = session();
        run(
            &core,
            &mut s,
            "CREATE TYPE mood AS ENUM ('a','b','c','d','e')",
        )
        .expect("enum");
        let mut cols = vec![
            "embedding VECTOR(2)".to_string(),
            "tags INTEGER[]".to_string(),
        ];
        cols.push("m mood".to_string());
        cols.extend((3..10).map(|i| format!("c{i} TEXT")));
        run(
            &core,
            &mut s,
            &format!("CREATE TABLE wide ({})", cols.join(", ")),
        )
        .expect("wide table");
        run(
            &core,
            &mut s,
            "INSERT INTO wide (id, embedding, tags) VALUES (1, '[0.1,0.2]', '{1,2,3}') \
             USING OPERATION_ID 'seed-1'",
        )
        .expect("seed");
    }
    // 列数 4・配列要素 2・ラベル 2 まで下げて開き直す（既存はすべて超過状態）。
    let core = core_at(&path, limits(4, 2, 2));
    let mut s = session();
    core.execute_sql(&ctx(), "SELECT id, tags, m, c3 FROM wide LIMIT 10")
        .expect("existing table remains readable");
    run(
        &core,
        &mut s,
        "INSERT INTO wide (id, embedding, tags) VALUES (2, '[0.1,0.2]', '{1,2,3,4,5}') \
         USING OPERATION_ID 'seed-2'",
    )
    .expect("existing array column keeps its persisted max_len");
    run(&core, &mut s, "ALTER TABLE wide DROP COLUMN c9").expect("drop still works");
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE wide ADD COLUMN extra TEXT"),
        "54000"
    );
    run(
        &core,
        &mut s,
        "CREATE TABLE uses_old_enum (embedding VECTOR(2), m mood)",
    )
    .expect("persisted 5-label enum stays usable");
}
