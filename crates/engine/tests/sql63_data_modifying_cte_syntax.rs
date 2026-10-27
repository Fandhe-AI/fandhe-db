//! データ変更 CTE の構文受理・列解決の結合テスト（SQL-63、Issue #1875）。ポインタ:
//! `docs/spec/04-behavior/sql-surface.md` SQL-63・`docs/spec/04-behavior/rls.md` RLS-10 (b)。
//!
//! #1875 の範囲は「CTE 本文の `INSERT`／`UPDATE`／`DELETE` を構文受理し、`RETURNING` の
//! 公開列を確定する」まで。書き込みの実行（最上位の文で副文 1 つを主クエリが読む形）は #1876
//! （`sql63_data_modifying_cte_exec.rs`）が担う。本ファイルは、実行しない形（RETURNING なしの参照・
//! 主クエリが実表を読む形・複数の副文）が検証を通過しても固定 `0A000` で止まり、**表の行は
//! 一切変わらない**ことを production 経路（`EngineCore::execute_sql_in_session`）で固定する。
//! エラー応答はテナント間で同一であること（他テナントの行の有無で応答が変わらない）も確認する。
//! 流儀は `sql29_cte.rs` と同じ。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
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
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(storage: &Storage, tenant: &str, id: u64, lang: &str) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        Visibility::Public,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(format!("{tenant} {id}")),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn open_seeded() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql63-dm-cte");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, "alice", 1, "ja");
    seed(&storage, "alice", 2, "en");
    seed(&storage, "bob", 3, "ja");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

/// テナントの可視行数（書き込みが起きていないことの観測用）。
fn count(core: &EngineCore, tenant: &str) -> usize {
    match run(core, tenant, "SELECT id FROM docs LIMIT 100").expect("count query") {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn lang_of_all(core: &EngineCore, tenant: &str) -> Vec<String> {
    match run(core, tenant, "SELECT lang FROM docs LIMIT 100").expect("lang query") {
        SqlOutcome::Query(result) => {
            let mut v: Vec<String> = result.rows.iter().map(|r| format!("{r:?}")).collect();
            v.sort();
            v
        }
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

#[test]
fn unsupported_data_modifying_cte_shapes_stop_with_0a000_and_change_nothing() {
    let (core, _guard) = open_seeded();
    let before_alice = (count(&core, "alice"), lang_of_all(&core, "alice"));
    let before_bob = (count(&core, "bob"), lang_of_all(&core, "bob"));
    for sql in [
        // 主クエリが実表を読む（文開始時点の可視性は #1877）
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja') SELECT * FROM docs LIMIT 10",
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT * FROM docs LIMIT 10",
        // RETURNING なしの副文を参照
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja') SELECT * FROM d LIMIT 10",
        // 複数のデータ変更 CTE（#1877）
        "WITH a AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id), \
         b AS (UPDATE docs SET lang = 'xx' WHERE lang = 'en' RETURNING id) \
         SELECT id FROM a LIMIT 10",
    ] {
        for tenant in ["alice", "bob"] {
            let err = run(&core, tenant, sql).expect_err("must stop before execution");
            assert_eq!(err.wire_code(), "0A000", "tenant={tenant} sql={sql}");
        }
        // 応答はテナント間で同一（他テナントの行の有無で変わらない）。
        let a = run(&core, "alice", sql).expect_err("alice").to_string();
        let b = run(&core, "bob", sql).expect_err("bob").to_string();
        assert_eq!(a, b, "sql={sql}");
    }
    assert_eq!(
        before_alice,
        (count(&core, "alice"), lang_of_all(&core, "alice"))
    );
    assert_eq!(before_bob, (count(&core, "bob"), lang_of_all(&core, "bob")));
}

#[test]
fn same_name_as_real_table_is_never_read() {
    let (core, _guard) = open_seeded();
    let before = count(&core, "alice");
    // CTE 名が実表と同名でも、実表の走査には化けず副文の RETURNING 行だけを返す
    // （実表を読めば公開行を含む複数行になる）。
    let outcome = run(
        &core,
        "alice",
        "WITH docs AS (DELETE FROM docs WHERE lang = 'ja' RETURNING *) SELECT * FROM docs LIMIT 10",
    )
    .expect("executes the data-modifying CTE");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    assert_eq!(result.rows.len(), 1, "only the deleted row is returned");
    // 実表が読まれていれば残るはずの行数ではなく、削除した 1 行分だけ減っている。
    assert_eq!(count(&core, "alice"), before - 1);
}

#[test]
fn error_priority_matches_unit_contract() {
    let (core, _guard) = open_seeded();
    let before = count(&core, "alice");
    let cases = [
        // 構文誤り
        ("WITH d AS (DELETE docs) SELECT * FROM d LIMIT 10", "42601"),
        // 副文の USING OPERATION_ID は不可
        (
            "WITH d AS (DELETE FROM docs WHERE lang = 'ja' USING OPERATION_ID 'op-1') \
             SELECT * FROM docs LIMIT 10",
            "42601",
        ),
        // 対象表なし
        (
            "WITH d AS (DELETE FROM ghost WHERE lang = 'ja' RETURNING *) SELECT * FROM d LIMIT 10",
            "42P01",
        ),
        // RETURNING の列なし
        (
            "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING nope) \
             SELECT * FROM d LIMIT 10",
            "42703",
        ),
        // RETURNING なしのデータ変更 CTE の参照
        (
            "WITH d AS (DELETE FROM docs WHERE lang = 'ja') SELECT * FROM d LIMIT 10",
            "0A000",
        ),
        // 主クエリが RETURNING に無い列を参照
        (
            "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
             SELECT * FROM d WHERE body = 'x' LIMIT 10",
            "42703",
        ),
    ];
    for (sql, code) in cases {
        let err = run(&core, "alice", sql).expect_err("must be rejected");
        assert_eq!(err.wire_code(), code, "sql={sql}");
    }
    assert_eq!(count(&core, "alice"), before);
}

#[test]
fn non_top_level_contexts_never_write() {
    let (core, _guard) = open_seeded();
    let before_alice = count(&core, "alice");
    let before_bob = count(&core, "bob");
    let body =
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING *) SELECT * FROM d LIMIT 10";
    for sql in [
        format!("DECLARE c CURSOR FOR {body}"),
        format!("COPY ({body}) TO STDOUT"),
        format!("CREATE VIEW v AS {body}"),
        format!("CREATE TABLE t2 AS {body}"),
        format!("EXPLAIN {body}"),
    ] {
        let err = run(&core, "alice", &sql).expect_err("must be rejected");
        assert!(
            ["0A000", "42601"].contains(&err.wire_code()),
            "code={} sql={sql}",
            err.wire_code()
        );
    }
    assert_eq!(count(&core, "alice"), before_alice);
    assert_eq!(count(&core, "bob"), before_bob);
}
