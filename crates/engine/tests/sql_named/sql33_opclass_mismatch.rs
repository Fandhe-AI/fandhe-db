//! 索引の距離種別（opclass）と距離演算子が合わないときの厳密全走査への縮退の結合テスト
//! （Issue #1979。ポインタ: INDEX-7・SQL-33・SQL-75、`docs/design/index-declaration-effects.md`）。
//!
//! 現状で不一致が起きるのは `HnswScope::Declared` で非 Cosine の HNSW 宣言（Rust API の
//! `Storage::create_index` で作る）しかない表へ `<=>` を問い合わせる場合。固定する内容:
//!
//! - 不一致では HNSW キャッシュへ一切照会せず、結果は宣言前の厳密結果と一致する
//! - `EXPLAIN` は `ann_plan: plain_scan_opclass_mismatch`（`index=` 注記なし）で、`metric=` は宣言側
//! - Cosine 宣言を足すと HNSW 経路へ戻り、DROP すると再び不一致になる
//! - `DESC` は不一致より優先する
//! - 縮退経路でもテナント境界（RLS 可視集合）を保つ

use engine::catalog::{ColumnDef, ColumnType, IndexDef, IndexKind, TableSchema, VectorOpClass};
use engine::core::{EngineCore, VectorCore};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine::{self, HnswScope};
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 8;
// `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超。
const HNSW_ROWS: usize = 1_100;
const TENANT_B_ID_BASE: u64 = 1_000_000;

fn gen_vectors(seed: u64, rows: usize) -> Vec<Vec<f32>> {
    let mut state = seed;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bits = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    (0..rows)
        .map(|_| (0..DIM).map(|_| next()).collect())
        .collect()
}

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(storage: &Storage, tenant: &str, id_base: u64, vis: Visibility, vectors: &[Vec<f32>]) {
    let schema = schema();
    let metadata: Vec<Vec<u8>> = (0..vectors.len())
        .map(|i| {
            let lang = if i % 3 == 0 { "ja" } else { "en" };
            engine::row_codec::encode_scalar_columns(
                &schema,
                &[Value::Null, Value::Text(lang.to_string())],
            )
            .expect("encode metadata")
        })
        .collect();
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                id_base + i as u64,
                RowInput {
                    tenant_id: tenant,
                    visibility: vis,
                    embedding: v.as_slice(),
                    metadata: metadata[i].as_slice(),
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(&format!("seed-{tenant}")).expect("valid operation id");
    engine::tenant::insert_rows(storage, "docs", &ctx(tenant), &rows, &op_id).expect("seed rows");
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

/// Declared scope の HNSW opt-in エンジン。tenant-a・tenant-b とも Private 行を持つ。
/// `l2_declared` なら `docs` に L2 の HNSW 宣言だけを置く（Rust API でのみ作れる非 Cosine 宣言）。
fn core_with_two_tenants(
    label: &str,
    l2_declared: bool,
) -> (EngineCore, CleanupGuard, Vec<Vec<f32>>) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create docs");
    let vectors_a = gen_vectors(3, HNSW_ROWS);
    let vectors_b = gen_vectors(5, HNSW_ROWS);
    seed(&storage, "tenant-a", 0, Visibility::Private, &vectors_a);
    seed(
        &storage,
        "tenant-b",
        TENANT_B_ID_BASE,
        Visibility::Private,
        &vectors_b,
    );
    if l2_declared {
        storage
            .create_index(&IndexDef::new(
                "idx_l2".to_string(),
                "docs".to_string(),
                IndexKind::Hnsw(VectorOpClass::L2),
                vec!["embedding".to_string()],
            ))
            .expect("declare l2 hnsw index");
    }
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core =
        EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(HnswScope::Declared);
    (core, guard, vectors_b)
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .expect("ddl");
}

fn hnsw_consulted(core: &EngineCore) -> u64 {
    let s = core.hnsw_index_cache_stats();
    s.hits + s.misses + s.builds + s.build_failures + s.fallbacks
}

fn explain_lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx(tenant), &mut session, &format!("EXPLAIN {sql}"))
        .expect("explain")
    {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .filter_map(|row| match row.cells.first() {
                Some(Cell::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

fn line<'a>(lines: &'a [String], prefix: &str) -> &'a str {
    lines
        .iter()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("{prefix} line missing: {lines:?}"))
}

/// 検索 SQL を実行し、HNSW キャッシュへ照会した回数の増分も返す。
fn run(core: &EngineCore, tenant: &str, sql: &str) -> (QueryResult, u64) {
    let before = hnsw_consulted(core);
    let result = core.execute_sql(&ctx(tenant), sql).expect("query");
    (result, hnsw_consulted(core) - before)
}

fn knn_sql(q: &[f32]) -> String {
    format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 5",
        vec_literal(q)
    )
}

/// 不一致（Declared で L2 宣言のみ）は厳密全走査で、結果は宣言前と一致し、EXPLAIN に縮退理由が出る。
/// Cosine 宣言を足すと HNSW へ戻り、DROP すると再び不一致になる。Subset 形（WHERE 付き）も同様。
#[test]
fn l2_only_declaration_falls_back_to_exact_scan_and_recovers_with_cosine() {
    let (baseline, _g0, _) = core_with_two_tenants("sql33-opclass-mismatch-base", false);
    let (core, _guard, vectors_b) = core_with_two_tenants("sql33-opclass-mismatch-basic", true);
    let q = &vectors_b[0];
    let sql = knn_sql(q);
    let subset_sql = format!(
        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{}' LIMIT 5",
        vec_literal(q)
    );

    // 宣言なし（Declared で宣言なし）は厳密。同一データの別エンジンを基準にする。
    let (exact, consulted) = run(&baseline, "tenant-b", &sql);
    assert_eq!(consulted, 0);
    let (exact_subset, _) = run(&baseline, "tenant-b", &subset_sql);

    let (mismatch, consulted) = run(&core, "tenant-b", &sql);
    assert_eq!(consulted, 0, "mismatch must not consult the hnsw cache");
    assert_eq!(mismatch, exact, "fallback result must equal exact scan");
    let (mismatch_subset, consulted) = run(&core, "tenant-b", &subset_sql);
    assert_eq!(consulted, 0);
    assert_eq!(mismatch_subset, exact_subset);

    let lines = explain_lines(&core, "tenant-b", &sql);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_opclass_mismatch"
    );
    assert!(
        line(&lines, "hnsw_params: ").ends_with("metric=l2"),
        "{lines:?}"
    );
    let lines = explain_lines(&core, "tenant-b", &subset_sql);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_opclass_mismatch"
    );

    // DESC は不一致より優先する。
    let desc_sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' DESC LIMIT 5",
        vec_literal(q)
    );
    let lines = explain_lines(&core, "tenant-b", &desc_sql);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_descending"
    );

    // Cosine 宣言を足すと HNSW 経路へ戻る。
    ddl(&core, "CREATE INDEX idx_cos ON docs USING hnsw (embedding)");
    let (_, consulted) = run(&core, "tenant-b", &sql);
    assert!(consulted > 0, "cosine declaration must enable hnsw");
    let lines = explain_lines(&core, "tenant-b", &sql);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: hnsw_full_visible index=idx_cos,idx_l2"
    );

    // Cosine 宣言を落とすと再び不一致（厳密）。
    ddl(&core, "DROP INDEX idx_cos");
    let (again, consulted) = run(&core, "tenant-b", &sql);
    assert_eq!(consulted, 0);
    assert_eq!(again, exact);
    let lines = explain_lines(&core, "tenant-b", &sql);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_opclass_mismatch"
    );
}

/// 縮退経路でもテナント境界を保つ: tenant-a の Private 行は tenant-b の結果に現れず、結果は
/// 宣言前の厳密結果と一致する。EXPLAIN にはテナント・行数の情報が出ない。
#[test]
fn fallback_path_keeps_tenant_boundary() {
    let (baseline, _g0, _) = core_with_two_tenants("sql33-opclass-mismatch-rls-base", false);
    let (core, _guard, vectors_b) = core_with_two_tenants("sql33-opclass-mismatch-rls", true);
    let sql = knn_sql(&vectors_b[1]);
    let (exact, _) = run(&baseline, "tenant-b", &sql);

    let (mismatch, consulted) = run(&core, "tenant-b", &sql);
    assert_eq!(consulted, 0);
    assert_eq!(mismatch, exact);
    assert_eq!(mismatch.rows.len(), 5);
    assert!(
        mismatch.rows.iter().all(|r| r.id >= TENANT_B_ID_BASE),
        "tenant-a rows must not leak: {:?}",
        mismatch.rows.iter().map(|r| r.id).collect::<Vec<_>>()
    );

    let lines = explain_lines(&core, "tenant-b", &sql);
    for l in &lines {
        assert!(!l.contains("tenant"), "{l}");
        assert!(!l.contains(&HNSW_ROWS.to_string()), "{l}");
    }
}

/// Rust API（`VectorCore::search`）も不一致では HNSW を使わず、宣言前の厳密結果と一致する。
#[test]
fn rust_api_search_falls_back_to_exact_on_mismatch() {
    let (baseline, _g0, _) = core_with_two_tenants("sql33-opclass-mismatch-api-base", false);
    let (core, _guard, vectors_b) = core_with_two_tenants("sql33-opclass-mismatch-api", true);
    let q = &vectors_b[2];
    let hits = |core: &EngineCore| -> Vec<(String, u64, u32)> {
        core.search(&ctx("tenant-b"), "docs", q, 5)
            .expect("search")
            .into_iter()
            .map(|h| (h.tenant_id, h.id, h.score.to_bits()))
            .collect()
    };
    let exact = hits(&baseline);
    let before = hnsw_consulted(&core);
    let mismatch = hits(&core);
    assert_eq!(hnsw_consulted(&core), before);
    assert_eq!(mismatch, exact);
    assert_eq!(mismatch.len(), 5);
}
