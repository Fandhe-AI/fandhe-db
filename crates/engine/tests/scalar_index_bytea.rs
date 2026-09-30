//! `BYTEA` 列の等価・範囲比較が二次索引経路（`OrderedColumnIndex::Bytes`）を
//! 使うことの SQL 表層結合テスト（Issue #1257。ポインタ: TABLE-13・INDEX-5）。
//!
//! `scalar_index_typed_range.rs`（契約 6 は `BYTEA` の索引消費を固定）とは別に、
//! 本ファイルは NULL 可の `BYTEA` 列・空バイト列・前方一致関係にある値・
//! 複合述語・RLS・世代進行・信頼マスク・集計・`EXPLAIN` を扱う。期待値は
//! Rust 側で `Vec<u8>` の辞書順比較から導出する全走査オラクルで、索引経路の
//! 結果と完全一致することを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "blob_docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("blob", ColumnType::Bytea, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

/// 行データ（`id` は 1 始まりの添字）。空バイト列・前方一致関係の値
/// （`\x07` と `\x0700`）・重複値・`NULL` を含む。`kind` は奇数 id が `a`。
fn rows() -> Vec<Option<Vec<u8>>> {
    vec![
        Some(vec![]),
        Some(vec![0x00]),
        Some(vec![0x07]),
        Some(vec![0x07, 0x00]),
        Some(vec![0x08]),
        None,
        Some(vec![0xff]),
        Some(vec![0xff, 0xff]),
        Some(vec![0x07]),
    ]
}

fn kind_of(id: u64) -> &'static str {
    if id % 2 == 1 {
        "a"
    } else {
        "b"
    }
}

fn insert_row(
    storage: &Storage,
    c: &PolicyContext,
    id: u64,
    blob: &Option<Vec<u8>>,
    visibility: Visibility,
    label: &str,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        c,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(kind_of(id).to_string()),
            blob.clone().map(Value::Bytes).unwrap_or(Value::Null),
        ],
        &op_id(label),
    )
    .expect("insert row");
}

fn seed(storage: &Storage, tenant: &str) {
    let c = ctx(tenant);
    for (i, blob) in rows().iter().enumerate() {
        let id = (i as u64) + 1;
        insert_row(
            storage,
            &c,
            id,
            blob,
            Visibility::Public,
            &format!("seed-{id}"),
        );
    }
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

fn ids(result: &QueryResult) -> Vec<u64> {
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn hex(b: &[u8]) -> String {
    let mut s = String::from("\\x");
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// 全走査オラクル: `pred` を満たす（`NULL` は常に不一致）id の昇順。
fn oracle(pred: impl Fn(&[u8]) -> bool) -> Vec<u64> {
    rows()
        .iter()
        .enumerate()
        .filter_map(|(i, b)| match b {
            Some(v) if pred(v) => Some((i as u64) + 1),
            _ => None,
        })
        .collect()
}

/// cold／hot の 2 回実行で結果が一致し、索引が消費され、期待 id 集合と
/// 一致することを固定する。
fn assert_indexed(core: &EngineCore, where_clause: &str, expected: &[u64]) {
    let sql = format!(
        "SELECT id FROM {TABLE} WHERE {where_clause} \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 50"
    );
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, "tenant-a", &sql);
    let hot = run(core, "tenant-a", &sql);
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(ids(&cold), ids(&hot), "cold/hot mismatch: {where_clause}");
    assert_eq!(ids(&hot), expected, "oracle mismatch: {where_clause}");
    // 候補比が選択度閾値（1/2 超）を超える述語は設計どおり全走査へ縮退するため、
    // 索引消費の検証は閾値以下（選択的）の述語に限る。結果の一致は常に検証する。
    if expected.len() * 2 <= 9 {
        assert!(after > before, "index not consumed: {where_clause}");
    }
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, "tenant-a");
    (new_core(storage), guard)
}

#[test]
fn equality_and_range_operators_use_index_and_match_oracle() {
    let (core, _guard) = open_core("scalar-index-bytea-ops");
    let probes: [&[u8]; 6] = [
        &[],
        &[0x07],
        &[0x07, 0x00],
        &[0x06],
        &[0xff, 0xff],
        &[0xff, 0xff, 0x01],
    ];
    for p in probes {
        let h = hex(p);
        assert_indexed(&core, &format!("blob = '{h}'"), &oracle(|v| v == p));
        assert_indexed(&core, &format!("blob < '{h}'"), &oracle(|v| v < p));
        assert_indexed(&core, &format!("blob <= '{h}'"), &oracle(|v| v <= p));
        assert_indexed(&core, &format!("blob > '{h}'"), &oracle(|v| v > p));
        assert_indexed(&core, &format!("blob >= '{h}'"), &oracle(|v| v >= p));
    }
}

#[test]
fn between_uses_index_and_matches_oracle() {
    let (core, _guard) = open_core("scalar-index-bytea-between");
    let lo: &[u8] = &[0x07];
    let hi: &[u8] = &[0x08];
    assert_indexed(
        &core,
        &format!("blob BETWEEN '{}' AND '{}'", hex(lo), hex(hi)),
        &oracle(|v| v >= lo && v <= hi),
    );
    // 逆順の範囲は 0 件（索引経路でも空集合として正しい）。
    assert_indexed(
        &core,
        &format!("blob BETWEEN '{}' AND '{}'", hex(hi), hex(lo)),
        &[],
    );
}

#[test]
fn empty_bytea_and_null_semantics() {
    let (core, _guard) = open_core("scalar-index-bytea-empty-null");
    assert_indexed(&core, "blob = '\\x'", &[1]);
    // NULL 行（id 6）は非 NULL 全行に含まれない。
    assert_indexed(&core, "blob >= '\\x'", &[1, 2, 3, 4, 5, 7, 8, 9]);
    assert_indexed(&core, "blob < '\\x'", &[]);
}

#[test]
fn conjunction_with_text_equality_intersects() {
    let (core, _guard) = open_core("scalar-index-bytea-conjunction");
    let expected: Vec<u64> = oracle(|v| v >= [0x07].as_slice())
        .into_iter()
        .filter(|id| kind_of(*id) == "a")
        .collect();
    assert_indexed(&core, "kind = 'a' AND blob >= '\\x07'", &expected);
}

#[test]
fn other_tenant_rows_never_leak_through_index() {
    let path = unique_db_path("scalar-index-bytea-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, "tenant-a");
    // tenant-b の private 行が tenant-a の述語に一致する値を持つ。
    insert_row(
        &storage,
        &ctx("tenant-b"),
        100,
        &Some(vec![0x07]),
        Visibility::Private,
        "seed-b-100",
    );
    let core = new_core(storage);
    assert_indexed(&core, "blob = '\\x07'", &[3, 9]);
    assert_indexed(
        &core,
        "blob >= '\\x07'",
        &oracle(|v| v >= [0x07].as_slice()),
    );
}

#[test]
fn generation_bump_rebuilds_bytea_index() {
    let (core, _guard) = open_core("scalar-index-bytea-generation");
    assert_indexed(&core, "blob = '\\x09'", &[]);
    let builds = core.scalar_index_cache_stats().builds;
    core.execute_insert_sql(
        &ctx("tenant-a"),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, kind, blob) \
             VALUES (20, '[20.0,0.0]', 'b', '\\x09') USING OPERATION_ID 'seed-20'"
        ),
    )
    .expect("insert");
    assert_indexed(&core, "blob = '\\x09'", &[20]);
    assert!(core.scalar_index_cache_stats().builds > builds);
}

#[test]
fn trusted_mask_and_count_star_match_oracle() {
    let (core, _guard) = open_core("scalar-index-bytea-mask-count");
    let expected = oracle(|v| v > [0x07].as_slice());
    let sql = format!(
        "SELECT id, kind FROM {TABLE} WHERE blob > '\\x07' \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 50"
    );
    let cold = run(&core, "tenant-a", &sql);
    let hot = run(&core, "tenant-a", &sql);
    assert_eq!(ids(&cold), expected);
    assert_eq!(ids(&hot), expected);

    let count_sql = format!("SELECT COUNT(*) AS n FROM {TABLE} WHERE blob > '\\x07'");
    let before = core.scalar_index_cache_stats().aggregate_index_scans;
    let cold = run(&core, "tenant-a", &count_sql);
    let hot = run(&core, "tenant-a", &count_sql);
    let after = core.scalar_index_cache_stats().aggregate_index_scans;
    let want = vec![Cell::Integer(expected.len() as u64)];
    let cells = |r: &QueryResult| r.rows.first().map(|row| row.cells.clone());
    assert_eq!(cells(&cold), Some(want.clone()));
    assert_eq!(cells(&hot), Some(want));
    assert!(after > before, "COUNT(*) must consume the BYTEA index");
}

fn explain(core: &EngineCore, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
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

#[test]
fn explain_reports_index_for_bytea_predicates() {
    let (core, _guard) = open_core("scalar-index-bytea-explain");
    let single = explain(
        &core,
        &format!(
            "EXPLAIN SELECT id FROM {TABLE} WHERE blob > '\\x07' \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
    );
    assert!(single.iter().any(|l| l == "scalar_plan: index_typed_range"));
    let conj = explain(
        &core,
        &format!(
            "EXPLAIN SELECT id FROM {TABLE} WHERE kind = 'a' AND blob > '\\x07' \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
    );
    assert!(conj.iter().any(|l| l == "scalar_plan: index_conjunction"));
    let in_list = explain(
        &core,
        &format!(
            "EXPLAIN SELECT id FROM {TABLE} WHERE blob IN ('\\x07', '\\x08') \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
    );
    assert!(in_list.iter().any(|l| l == "scalar_plan: plain_scan"));
}
