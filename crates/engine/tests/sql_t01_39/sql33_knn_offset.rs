//! kNN（ベクトル順位付け検索 SELECT）の `OFFSET`（Issue #2008。ポインタ: SQL-33 (8)・
//! SQL-34 (1)・SQL-46 (6)・SQL-73 (3)。設計は `docs/design/sql-offset-paging.md`）の結合テスト。
//!
//! 契約は恒等式「`LIMIT k OFFSET m` の結果 = 同じ経路の `LIMIT m+k` の結果から先頭 m 件を除いた
//! もの」で固定する（距離 ORDER BY・HYBRID・precision・`WHERE` 事前フィルタ・`HINT ORDER`）。
//! 先頭 m 件の除去は RLS 安全網の後で行うため、他テナントの不可視行は OFFSET の計数に入らない。
//! HNSW 経路との整合は `core_misc/hnsw_cache.rs`、設定値連動は `core_misc/result_limits.rs`。

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

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert(storage: &Storage, tenant: &str, id: u64, x: f32, lang: &str, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![x, 1.0]),
            Value::Text(format!("term{id} common")),
            Value::Text(lang.to_string()),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("op id"),
    )
    .expect("insert row");
}

/// tenant-a の可視 20 行。x は 1..=10 を 2 行ずつ（同距離のタイブレークを含む）。
fn core_with_corpus(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for id in 1..=20u64 {
        let x = id.div_ceil(2) as f32;
        let lang = if id % 2 == 0 { "ja" } else { "en" };
        insert(&storage, "tenant-a", id, x, lang, Visibility::Public);
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    match run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e:?}")) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected query outcome, got {other:?}"),
    }
}

fn ids(core: &EngineCore, tenant: &str, sql: &str) -> Vec<u64> {
    query(core, tenant, sql).rows.iter().map(|r| r.id).collect()
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

type Template = fn(usize, Option<usize>) -> String;

fn off(offset: Option<usize>) -> String {
    offset.map_or(String::new(), |m| format!(" OFFSET {m}"))
}

fn knn(limit: usize, offset: Option<usize>) -> String {
    format!(
        "SELECT id FROM docs ORDER BY embedding <=> '[4.2,1]' LIMIT {limit}{}",
        off(offset)
    )
}

fn knn_where(limit: usize, offset: Option<usize>) -> String {
    format!(
        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[4.2,1]' LIMIT {limit}{}",
        off(offset)
    )
}

fn knn_hint(limit: usize, offset: Option<usize>) -> String {
    format!(
        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[4.2,1]' LIMIT {limit}{} HINT ORDER(DISTANCE, SCALAR, RLS)",
        off(offset)
    )
}

fn hybrid(limit: usize, offset: Option<usize>) -> String {
    format!(
        "SELECT id FROM docs ORDER BY HYBRID(embedding, '[4.2,1]', body, 'common') LIMIT {limit}{}",
        off(offset)
    )
}

fn hybrid_rrf(limit: usize, offset: Option<usize>) -> String {
    format!(
        "SELECT id FROM docs ORDER BY HYBRID_RRF(embedding, '[4.2,1]', body, 'common') LIMIT {limit}{}",
        off(offset)
    )
}

fn precision(limit: usize, offset: Option<usize>) -> String {
    format!(
        "SELECT id FROM docs ORDER BY embedding <=> '[4.2,1]' LIMIT {limit}{} USING MODE 'precision'",
        off(offset)
    )
}

/// 恒等式 `LIMIT k OFFSET m` == `LIMIT m+k` の `[m..]`（順序まで一致）を複数の (k, m) で確かめる。
fn assert_identity(core: &EngineCore, template: Template) {
    for (k, m) in [(1, 1), (3, 0), (3, 2), (5, 5), (4, 9), (2, 15), (10, 3)] {
        let full = ids(core, "tenant-a", &template(m + k, None));
        let want: Vec<u64> = full.into_iter().skip(m).collect();
        let got = ids(core, "tenant-a", &template(k, Some(m)));
        assert_eq!(got, want, "k={k} m={m} sql={}", template(k, Some(m)));
    }
}

#[test]
fn identity_holds_for_distance_where_hint_hybrid_and_precision() {
    let (core, _guard) = core_with_corpus("sql33-knn-offset-identity");
    assert_identity(&core, knn);
    assert_identity(&core, knn_where);
    assert_identity(&core, knn_hint);
    assert_identity(&core, hybrid);
    assert_identity(&core, hybrid_rrf);
    assert_identity(&core, precision);
}

#[test]
fn paging_concatenation_matches_full_ranking_without_gaps_or_duplicates() {
    let (core, _guard) = core_with_corpus("sql33-knn-offset-paging");
    let full = ids(&core, "tenant-a", &knn(20, None));
    assert_eq!(full.len(), 20);
    let mut paged = Vec::new();
    for page in 0..4 {
        paged.extend(ids(&core, "tenant-a", &knn(5, Some(page * 5))));
    }
    assert_eq!(paged, full);
}

#[test]
fn offset_zero_equals_omitted_and_beyond_visible_is_empty_with_columns() {
    let (core, _guard) = core_with_corpus("sql33-knn-offset-edge");
    assert_eq!(
        ids(&core, "tenant-a", &knn(5, Some(0))),
        ids(&core, "tenant-a", &knn(5, None))
    );
    let empty = query(&core, "tenant-a", &knn(5, Some(20)));
    assert!(empty.rows.is_empty());
    assert_eq!(empty.columns.len(), 1);
    assert!(query(&core, "tenant-a", &knn(5, Some(500))).rows.is_empty());
}

#[test]
fn fetch_first_form_matches_limit_offset() {
    let (core, _guard) = core_with_corpus("sql33-knn-offset-fetch");
    let fetch = ids(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY embedding <=> '[4.2,1]' OFFSET 3 ROWS FETCH FIRST 4 ROWS ONLY",
    );
    assert_eq!(fetch, ids(&core, "tenant-a", &knn(4, Some(3))));
}

/// 他テナントの Private 行を最近傍位置へ大量に置いても OFFSET の計数・結果に現れない。
#[test]
fn offset_does_not_count_or_leak_other_tenant_rows() {
    let path = unique_db_path("sql33-knn-offset-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for id in 1..=8u64 {
        insert(&storage, "tenant-0", id, 4.2, "ja", Visibility::Private);
    }
    for id in 101..=106u64 {
        insert(
            &storage,
            "tenant-a",
            id,
            (id - 100) as f32,
            "ja",
            Visibility::Public,
        );
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let full = ids(&core, "tenant-a", &knn(6, None));
    assert_eq!(full.len(), 6);
    assert!(full.iter().all(|id| *id >= 101));
    assert_eq!(
        ids(&core, "tenant-a", &knn(3, Some(2))),
        full[2..5].to_vec()
    );
    assert_eq!(
        ids(&core, "tenant-a", &knn(10, Some(4))),
        full[4..].to_vec()
    );
    assert!(ids(&core, "tenant-a", &knn(3, Some(6))).is_empty());
}

#[test]
fn range_and_total_validation() {
    let (core, _guard) = core_with_corpus("sql33-knn-offset-range");
    assert_eq!(code(&core, "tenant-a", &knn(1, Some(10_001))), "22000");
    assert_eq!(code(&core, "tenant-a", &knn(1, Some(10_000))), "54000");
    assert_eq!(code(&core, "tenant-a", &knn(5_000, Some(5_001))), "54000");
    assert!(query(&core, "tenant-a", &knn(5_000, Some(5_000)))
        .rows
        .is_empty());
    assert_eq!(code(&core, "tenant-a", &knn(0, Some(1))), "22000");
}

#[test]
fn malformed_forms_are_syntax_errors() {
    let (core, _guard) = core_with_corpus("sql33-knn-offset-syntax");
    for sql in [
        "SELECT id FROM docs ORDER BY embedding <=> '[4.2,1]' OFFSET 1 LIMIT 5",
        "SELECT id FROM docs ORDER BY embedding <=> '[4.2,1]' LIMIT 5 OFFSET -1",
        "SELECT id FROM docs ORDER BY embedding <=> '[4.2,1]' LIMIT 5 OFFSET 1 junk",
        "SELECT id FROM docs USING PLAN('q') LIMIT 5 OFFSET 1",
    ] {
        assert_eq!(code(&core, "tenant-a", sql), "42601", "{sql}");
    }
}

#[test]
fn explain_accepts_offset_and_reports_the_same_plan() {
    let (core, _guard) = core_with_corpus("sql33-knn-offset-explain");
    let with = run(&core, "tenant-a", &format!("EXPLAIN {}", knn(3, Some(2)))).expect("explain");
    let without = run(&core, "tenant-a", &format!("EXPLAIN {}", knn(3, None))).expect("explain");
    assert_eq!(format!("{with:?}"), format!("{without:?}"));
}
