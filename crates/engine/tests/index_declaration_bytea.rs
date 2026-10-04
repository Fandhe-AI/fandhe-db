//! `BYTEA` 列への `CREATE INDEX` 宣言の受理（Issue #1362・TASK-206・INDEX-7・
//! TABLE-13）の結合テスト。
//!
//! `tests/scalar_index_bytea.rs`（Issue #1257。自動構築での `BYTEA` 索引化）と
//! `tests/index_declaration_targets.rs`（宣言が構築対象へ効くこと）を補完し、
//! 宣言付きテーブル（HNSW opt-in）でも宣言した `BYTEA` 列が索引経路を使うこと、
//! 宣言外の列は従来どおり縮退すること、`EXPLAIN` の索引名が他型と同じ規則で
//! 付くこと、`Private` 行がテナントを越えないことを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;
use engine::storage::Visibility;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超。
const ROWS: usize = 1_100;

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("blob", ColumnType::Bytea, true),
            ColumnDef::new("day", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 行 `i` の `blob`（2 バイト。値は 0..100 を巡回し、述語が選択的になる）。
fn blob_of(i: usize) -> Vec<u8> {
    vec![0, (i % 100) as u8]
}

fn insert(storage: &Storage, id: u64, vis: Visibility, blob: Vec<u8>) {
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx("tenant-a"),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 1.0]),
            Value::Text(if id.is_multiple_of(2) { "a" } else { "b" }.to_string()),
            Value::Bytes(blob),
            Value::Text(format!("d{:03}", id % 100)),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for i in 0..ROWS {
        insert(&storage, i as u64, Visibility::Public, blob_of(i));
    }
    // テナント境界確認用の Private 行（blob = 0x0007 と一致）。
    insert(&storage, ROWS as u64, Visibility::Private, blob_of(7));
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    (EngineCore::from_storage_with_engine(storage, kind), guard)
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("DDL must succeed: {sql}: {e:?}"));
}

fn ids(core: &EngineCore, tenant: &str, where_clause: &str) -> Vec<u64> {
    let sql = format!(
        "SELECT id FROM docs WHERE {where_clause} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2000"
    );
    let result = core
        .execute_sql(&ctx(tenant), &sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"));
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn explain(core: &EngineCore, where_clause: &str) -> Vec<String> {
    let sql = format!(
        "EXPLAIN SELECT id FROM docs WHERE {where_clause} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
    );
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx("tenant-a"), &mut session, &sql)
        .expect("EXPLAIN must succeed")
    {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(Cell::Text(s)) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

/// 全走査オラクル（`Public` 行のみ。`pred` を満たす id の昇順）。
fn oracle(pred: impl Fn(&[u8]) -> bool) -> Vec<u64> {
    (0..ROWS)
        .filter(|i| pred(&blob_of(*i)))
        .map(|i| i as u64)
        .collect()
}

/// 宣言した `BYTEA` 列の述語は宣言付きテーブルでも索引経路を使い、全走査
/// オラクルと一致する。
#[test]
fn declared_bytea_column_uses_index_and_matches_oracle() {
    let (core, _guard) = open_core("idx-decl-bytea-use");
    ddl(&core, "CREATE INDEX idx_blob ON docs (blob)");
    let cases: Vec<(&str, Vec<u64>)> = vec![
        ("blob = '\\x0007'", oracle(|b| b == [0, 7])),
        ("blob >= '\\x0062'", oracle(|b| b >= [0u8, 0x62].as_slice())),
        (
            "blob BETWEEN '\\x0003' AND '\\x0004'",
            oracle(|b| b >= [0u8, 3].as_slice() && b <= [0u8, 4].as_slice()),
        ),
    ];
    for (clause, expected) in cases {
        let _ = ids(&core, "tenant-a", clause);
        let before = core.scalar_index_cache_stats().index_scans;
        let got = ids(&core, "tenant-a", clause);
        let after = core.scalar_index_cache_stats();
        // tenant-a は Private 行（id = ROWS）も見えるため、それを除いて比較する。
        let got: Vec<u64> = got.into_iter().filter(|id| *id != ROWS as u64).collect();
        assert_eq!(got, expected, "oracle mismatch: {clause}");
        assert!(after.index_scans > before, "index not consumed: {clause}");
    }
}

/// 補集合: `BYTEA` を宣言していなければ宣言外として縮退する（ゲートを緩めない）。
#[test]
fn undeclared_bytea_column_falls_back_to_plain_scan() {
    let (core, _guard) = open_core("idx-decl-bytea-undeclared");
    ddl(&core, "CREATE INDEX idx_kind ON docs (kind)");
    let clause = "blob = '\\x0007'";
    let _ = ids(&core, "tenant-a", clause);
    let before = core.scalar_index_cache_stats().plain_scan_fallbacks;
    let got = ids(&core, "tenant-a", clause);
    let after = core.scalar_index_cache_stats().plain_scan_fallbacks;
    assert!(after > before, "undeclared BYTEA must fall back");
    let got: Vec<u64> = got.into_iter().filter(|id| *id != ROWS as u64).collect();
    assert_eq!(got, oracle(|b| b == [0, 7]));
}

/// RLS: 他テナントの `Private` 行は索引経路でも見えない。
#[test]
fn private_rows_do_not_leak_through_declared_bytea_index() {
    let (core, _guard) = open_core("idx-decl-bytea-rls");
    ddl(&core, "CREATE INDEX idx_blob ON docs (blob)");
    let clause = "blob = '\\x0007'";
    let _ = ids(&core, "tenant-a", clause);
    let owner = ids(&core, "tenant-a", clause);
    assert!(owner.contains(&(ROWS as u64)));
    let _ = ids(&core, "tenant-b", clause);
    let other = ids(&core, "tenant-b", clause);
    assert!(!other.contains(&(ROWS as u64)), "private row leaked");
    assert_eq!(other, oracle(|b| b == [0, 7]));
}

/// `EXPLAIN` の索引名は `BYTEA` でも他型（TEXT 範囲）と同じ規則で付く。
#[test]
fn explain_index_names_follow_same_rule_as_other_types() {
    let (core, _guard) = open_core("idx-decl-bytea-explain");
    ddl(&core, "CREATE INDEX idx_blob ON docs (blob)");
    ddl(&core, "CREATE INDEX idx_kind ON docs (kind)");
    let has = |lines: &[String], want: &str| lines.iter().any(|l| l == want);
    let blob = explain(&core, "blob > '\\x0007'");
    assert!(
        has(&blob, "scalar_plan: index_typed_range index=idx_blob"),
        "{blob:?}"
    );
    let conj = explain(&core, "kind = 'a' AND blob > '\\x0007'");
    assert!(
        has(
            &conj,
            "scalar_plan: index_conjunction index=idx_blob,idx_kind"
        ),
        "{conj:?}"
    );
    // BYTEA の IN は本 Issue の対象外（従来どおり plain_scan）。
    let in_list = explain(&core, "blob IN ('\\x0007', '\\x0008')");
    assert!(
        in_list
            .iter()
            .any(|l| l.starts_with("scalar_plan: plain_scan")),
        "{in_list:?}"
    );
}
