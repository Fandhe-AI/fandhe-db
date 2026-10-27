//! CTE の `MATERIALIZED`／`NOT MATERIALIZED` 指定の結合テスト（SQL-62 (3)・Issue #1858、
//! 親 Issue #1856）。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-62 (3)・RLS-10 (b)。
//!
//! 指定は構文として受理して読み捨てる（現行は常にインライン展開）ため、指定なし・
//! `MATERIALIZED`・`NOT MATERIALIZED` の 3 変種で結果（列名・行 ID・セル値）が同一であること、
//! テナント境界が不変であること、不正形が `42601` で拒否されることを production 経路
//! （`EngineCore::execute_sql_in_session`）で確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
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

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn query_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    query(core, tenant, sql).expect_err("must be rejected")
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

/// alice: public 3 件（ja 2・en 1）・private 1 件（ja）。bob: public 1 件
/// （ja）・private 1 件（ja）。carol: public 1 件（ja）。
/// `table18_view.rs::seed_base_fixture` と同一のフィクスチャ構成
/// （テナント境界検証を同条件で比較できるようにするため）。
fn seed_base_fixture(storage: &Storage) {
    insert_row(
        storage,
        &ctx("alice"),
        1,
        "ja",
        "alice public ja 1",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        2,
        "ja",
        "alice public ja 2",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        3,
        "en",
        "alice public en",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        4,
        "ja",
        "alice private ja",
        Visibility::Private,
    );
    insert_row(
        storage,
        &ctx("bob"),
        5,
        "ja",
        "bob public ja",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("bob"),
        6,
        "ja",
        "bob private ja",
        Visibility::Private,
    );
    insert_row(
        storage,
        &ctx("carol"),
        7,
        "ja",
        "carol public ja",
        Visibility::Public,
    );
}

fn open_seeded() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-cte-col");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    (new_core(storage), guard)
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query_err(core, tenant, sql).wire_code().to_string()
}

fn render(result: &QueryResult) -> String {
    format!("{:?} {:?}", result.columns, result.rows)
}

/// `{h}` を 3 変種（指定なし・`MATERIALIZED`・`NOT MATERIALIZED`）へ置換した SQL を返す。
fn variants(template: &str) -> [String; 3] {
    ["", "MATERIALIZED ", "NOT MATERIALIZED "].map(|h| template.replace("{h}", h))
}

fn assert_same_across_hints(core: &EngineCore, tenant: &str, template: &str) {
    let [plain, mat, not_mat] = variants(template);
    let base = query(core, tenant, &plain).unwrap_or_else(|e| panic!("{plain}: {e:?}"));
    assert!(!base.rows.is_empty(), "fixture must yield rows: {plain}");
    for sql in [mat, not_mat] {
        let r = query(core, tenant, &sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        assert_eq!(render(&r), render(&base), "sql={sql}");
    }
}

#[test]
fn hint_does_not_change_results() {
    let (core, _g) = open_seeded();
    for template in [
        "WITH x AS {h}(SELECT * FROM docs) SELECT * FROM x LIMIT 100",
        "WITH x AS {h}(SELECT id, lang FROM docs WHERE lang = 'ja') \
         SELECT id, lang FROM x WHERE id > 1 LIMIT 100",
        "WITH x AS {h}(SELECT lang FROM docs) SELECT lang, count(*) FROM x GROUP BY lang",
        "WITH a AS {h}(SELECT lang, body FROM docs), b AS {h}(SELECT lang FROM a) \
         SELECT lang FROM b LIMIT 100",
        "WITH x (l) AS {h}(SELECT lang FROM docs) SELECT l FROM x LIMIT 100",
    ] {
        assert_same_across_hints(&core, "alice", template);
    }
}

#[test]
fn hint_keeps_tenant_isolation() {
    let (core, _g) = open_seeded();
    let expected = [
        ("alice", vec![1, 2, 3, 4, 5, 7]),
        ("bob", vec![1, 2, 3, 5, 6, 7]),
    ];
    for (tenant, _) in &expected {
        assert_same_across_hints(
            &core,
            tenant,
            "WITH x AS {h}(SELECT * FROM docs) SELECT * FROM x LIMIT 100",
        );
    }
    // 他テナントの private 行はヒント付きでも見えない（指定なしと同一の可視行集合）。
    let [_, mat, _] = variants("WITH x AS {h}(SELECT * FROM docs) SELECT * FROM x LIMIT 100");
    let alice = query(&core, "alice", &mat).expect("alice");
    let carol = query(&core, "carol", &mat).expect("carol");
    assert!(!result_ids(&alice).contains(&6));
    assert!(!result_ids(&carol).contains(&4));
    assert!(!result_ids(&carol).contains(&6));
}

#[test]
fn unreferenced_cte_with_hint_is_still_validated() {
    let (core, _g) = open_seeded();
    let [plain, mat, not_mat] =
        variants("WITH x AS {h}(SELECT * FROM missing_table) SELECT * FROM docs LIMIT 10");
    let base = code(&core, "alice", &plain);
    assert_eq!(code(&core, "alice", &mat), base);
    assert_eq!(code(&core, "alice", &not_mat), base);
}

#[test]
fn malformed_hint_is_42601() {
    let (core, _g) = open_seeded();
    for sql in [
        "WITH x AS NOT (SELECT * FROM docs) SELECT * FROM x LIMIT 10",
        "WITH x AS NOT NOT MATERIALIZED (SELECT * FROM docs) SELECT * FROM x LIMIT 10",
        "WITH x AS MATERIALIZED MATERIALIZED (SELECT * FROM docs) SELECT * FROM x LIMIT 10",
        "WITH x MATERIALIZED AS (SELECT * FROM docs) SELECT * FROM x LIMIT 10",
        "WITH x AS (SELECT * FROM docs) MATERIALIZED SELECT * FROM x LIMIT 10",
    ] {
        assert_eq!(code(&core, "alice", sql), "42601", "sql={sql}");
    }
}

#[test]
fn view_body_with_cte_hint_matches_plain_view() {
    let (core, _g) = open_seeded();
    let mut session = SessionState::default();
    let c = ctx("alice");
    let create = |name: &str, h: &str| {
        format!("CREATE VIEW {name} AS WITH x AS {h}(SELECT * FROM docs) SELECT * FROM x")
    };
    let mut outcomes = Vec::new();
    for (name, h) in [
        ("v_plain", ""),
        ("v_mat", "MATERIALIZED "),
        ("v_not", "NOT MATERIALIZED "),
    ] {
        outcomes.push(
            core.execute_sql_in_session(&c, &mut session, &create(name, h))
                .map(|_| ())
                .map_err(|e| e.wire_code().to_string()),
        );
    }
    // ヒントの有無で成功／失敗（エラーコード）が一致すること。
    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(outcomes[0], outcomes[2]);
    if outcomes[0].is_ok() {
        let plain = query(&core, "alice", "SELECT * FROM v_plain LIMIT 100").expect("plain view");
        for v in ["v_mat", "v_not"] {
            let r = query(&core, "alice", &format!("SELECT * FROM {v} LIMIT 100")).expect(v);
            assert_eq!(render(&r), render(&plain), "view={v}");
        }
    }
}
