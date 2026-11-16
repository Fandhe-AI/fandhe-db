//! HNSW のゲートと物理グラフの写像に内積（`vector_ip_ops`）を加えた経路の結合テスト
//! （Issue #3303。ポインタ: INDEX-7・SQL-33・SQL-75、`docs/design/hnsw-generation-cache.md`）。
//!
//! 固定する内容:
//!
//! - `vector_ip_ops` を宣言した表の `<#> ... LIMIT` は HNSW キャッシュへ照会され（`EXPLAIN` は
//!   `hnsw_full_visible`／`hnsw_subset`）、距離値は `-dot` の昇順。末尾 `;` 付きでも同じ
//! - 宣言なし（`All` の暗黙コサインのみ・`Declared` で cosine だけ）の `<#>` は従来どおり厳密全走査
//! - DESC・LIMIT 省略・`precision` 併用・`<->`／`<+>` は HNSW より前に縮退または拒否される
//! - テナント境界を保つ（他テナントの行は結果にも `EXPLAIN` にも現れない）
//!
//! Issue #3305 で追加する内容:
//!
//! - コサインの SQL 表層試験（`core_misc/hnsw_cache.rs`）と同じ手順・閾値の Recall@10 下限（常駐精度別）
//! - 内積が最大になるカナリア行を他テナントへ置いた越境試験（ctx ごとにキャッシュが別になることも含む）
//! - 再起動（`Storage` の再オープン）後に宣言 opclass からゲート・写像・キャッシュキーが復元され、
//!   `<#>` が HNSW を使うこと。fail-closed の形は再起動後も保たれる

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine::{self, HnswScope};
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};
use std::path::{Path, PathBuf};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 8;
// `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超。
const HNSW_ROWS: usize = 1_100;
const TENANT_B_ID_BASE: u64 = 1_000_000;
// tenant-a のカナリア行（tenant-b のクエリの定数倍のベクトル）の id 起点。
const CANARY_ID_BASE: u64 = 500_000;

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
    schema_dim(DIM)
}

fn schema_dim(dim: u32) -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(storage: &Storage, tenant: &str, id_base: u64, vis: Visibility, vectors: &[Vec<f32>]) {
    seed_with(storage, tenant, id_base, vis, vectors, DIM, tenant);
}

/// 次元と操作 ID の接尾辞を指定できる `seed`（Recall 用コーパス・カナリア行の追加で使う）。
fn seed_with(
    storage: &Storage,
    tenant: &str,
    id_base: u64,
    vis: Visibility,
    vectors: &[Vec<f32>],
    dim: u32,
    op_label: &str,
) {
    let schema = schema_dim(dim);
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
    let op_id = OperationId::parse(&format!("seed-{op_label}")).expect("valid operation id");
    engine::tenant::insert_rows(storage, "docs", &ctx(tenant), &rows, &op_id).expect("seed rows");
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

/// HNSW opt-in エンジン（`scope` 指定）。tenant-a・tenant-b とも Private 行を持つ。宣言は呼び出し側が DDL で足す。
fn core_with_two_tenants(
    label: &str,
    scope: HnswScope,
) -> (EngineCore, CleanupGuard, Vec<Vec<f32>>) {
    let (core, guard, _path, vectors_b) = core_with_two_tenants_at(label, scope, &[]);
    (core, guard, vectors_b)
}

/// [`core_with_two_tenants`] に DB パスと、tenant-a の追加カナリア行（Private・id は `CANARY_ID_BASE` 起点）を足した版。
/// 再起動試験が同じパスを開き直すために `path` を返す。
fn core_with_two_tenants_at(
    label: &str,
    scope: HnswScope,
    canaries_a: &[Vec<f32>],
) -> (EngineCore, CleanupGuard, PathBuf, Vec<Vec<f32>>) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create docs");
    let vectors_a = gen_vectors(3, HNSW_ROWS);
    let vectors_b = gen_vectors(5, HNSW_ROWS);
    seed(&storage, "tenant-a", 0, Visibility::Private, &vectors_a);
    if !canaries_a.is_empty() {
        seed_with(
            &storage,
            "tenant-a",
            CANARY_ID_BASE,
            Visibility::Private,
            canaries_a,
            DIM,
            "canary-a",
        );
    }
    seed(
        &storage,
        "tenant-b",
        TENANT_B_ID_BASE,
        Visibility::Private,
        &vectors_b,
    );
    let core = core_from_storage(storage, scope);
    (core, guard, path, vectors_b)
}

fn core_from_storage(storage: Storage, scope: HnswScope) -> EngineCore {
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(scope)
}

/// 再起動の模擬: 既存パスの `Storage` を開き直して新しい `EngineCore`（ゲートキャッシュ・HNSW キャッシュは空）を作る。
fn reopen(path: &Path, scope: HnswScope) -> EngineCore {
    core_from_storage(Storage::open(path).expect("reopen storage"), scope)
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

fn knn_ip_sql(q: &[f32]) -> String {
    format!(
        "SELECT id FROM docs ORDER BY embedding <#> '{}' LIMIT 5",
        vec_literal(q)
    )
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum()
}

fn err_code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    core.execute_sql(&ctx(tenant), sql)
        .expect_err("must be rejected")
        .wire_code()
        .to_string()
}

/// ip 宣言の表の `<#>` は HNSW を照会し、距離は `-dot` の昇順、Recall は下限以上、EXPLAIN は index 注記つき。
#[test]
fn ip_declaration_routes_negative_inner_product_to_hnsw() {
    let (baseline, _g0, _) = core_with_two_tenants("sql33-ip-base", HnswScope::Declared);
    let (core, _g, vectors_b) = core_with_two_tenants("sql33-ip-basic", HnswScope::Declared);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );

    let mut found = 0usize;
    let mut total = 0usize;
    for qi in 0..5 {
        let q = &vectors_b[qi * 7];
        let sql = knn_ip_sql(q);
        let (exact, consulted) = run(&baseline, "tenant-b", &sql);
        assert_eq!(consulted, 0);
        let (got, consulted) = run(&core, "tenant-b", &sql);
        assert!(consulted > 0, "ip declaration must reach hnsw");
        assert_eq!(got.rows.len(), 5);
        assert!(got.rows.iter().all(|r| r.id >= TENANT_B_ID_BASE));
        for r in &got.rows {
            let v = &vectors_b[(r.id - TENANT_B_ID_BASE) as usize];
            assert!(
                (r.score + dot(q, v)).abs() < 1e-4,
                "score must be -dot: {} vs {}",
                r.score,
                -dot(q, v)
            );
        }
        assert!(got.rows.windows(2).all(|w| w[0].score <= w[1].score));
        let want: std::collections::BTreeSet<u64> = exact.rows.iter().map(|r| r.id).collect();
        found += got.rows.iter().filter(|r| want.contains(&r.id)).count();
        total += want.len();
    }
    assert!(found * 10 >= total * 7, "recall {found}/{total}");

    let lines = explain_lines(&core, "tenant-b", &knn_ip_sql(&vectors_b[0]));
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: hnsw_full_visible index=idx_ip"
    );
    assert!(
        line(&lines, "hnsw_params: ").contains("metric=ip,"),
        "{lines:?}"
    );
}

/// 末尾 `;` 付きの単独送信（psql 相当）でも HNSW を照会し、`;` なしと同じ結果になる。
#[test]
fn trailing_semicolon_reaches_hnsw_with_same_result() {
    let (core, _g, vectors_b) = core_with_two_tenants("sql33-ip-semicolon", HnswScope::Declared);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    let sql = knn_ip_sql(&vectors_b[3]);
    let (plain, c1) = run(&core, "tenant-b", &sql);
    assert!(c1 > 0);
    let (semi, c2) = run(&core, "tenant-b", &format!("{sql};"));
    assert!(c2 > 0);
    assert_eq!(plain, semi);
    let mut session = SessionState::default();
    let before = hnsw_consulted(&core);
    match core
        .execute_sql_in_session(&ctx("tenant-b"), &mut session, &format!("{sql};"))
        .expect("session query")
    {
        SqlOutcome::Query(r) => assert_eq!(r, plain),
        other => panic!("expected query outcome, got {other:?}"),
    }
    assert!(hnsw_consulted(&core) > before);
}

/// 宣言なし・cosine だけの宣言では `<#>` は厳密全走査（HNSW 非照会）。ip 宣言を DROP すると戻る。
#[test]
fn without_ip_declaration_negative_inner_product_stays_exact() {
    let (all, _g0, vb) = core_with_two_tenants("sql33-ip-all", HnswScope::All);
    let sql = knn_ip_sql(&vb[0]);
    let (_, consulted) = run(&all, "tenant-b", &sql);
    assert_eq!(consulted, 0, "implicit cosine hnsw must not serve <#>");
    let lines = explain_lines(&all, "tenant-b", &sql);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_opclass_mismatch"
    );

    let (decl, _g1, vb) = core_with_two_tenants("sql33-ip-cos-only", HnswScope::Declared);
    ddl(&decl, "CREATE INDEX idx_cos ON docs USING hnsw (embedding)");
    let (exact, consulted) = run(&decl, "tenant-b", &sql);
    assert_eq!(consulted, 0);
    let lines = explain_lines(&decl, "tenant-b", &sql);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_opclass_mismatch"
    );

    ddl(
        &decl,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    let (_, consulted) = run(&decl, "tenant-b", &knn_ip_sql(&vb[0]));
    assert!(consulted > 0);
    ddl(&decl, "DROP INDEX idx_ip");
    let (again, consulted) = run(&decl, "tenant-b", &sql);
    assert_eq!(consulted, 0);
    assert_eq!(again, exact);
}

/// Subset 形（SCALAR 事前フィルタ付き）も HNSW に載り、結果は条件を満たす行だけで `-dot` 昇順。
#[test]
fn subset_shape_uses_hnsw_for_negative_inner_product() {
    let (core, _g, vb) = core_with_two_tenants("sql33-ip-subset", HnswScope::Declared);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    let sql = format!(
        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <#> '{}' LIMIT 5",
        vec_literal(&vb[0])
    );
    let (got, consulted) = run(&core, "tenant-b", &sql);
    assert!(consulted > 0);
    assert!(!got.rows.is_empty());
    assert!(got
        .rows
        .iter()
        .all(|r| r.id >= TENANT_B_ID_BASE && (r.id - TENANT_B_ID_BASE).is_multiple_of(3)));
    assert!(got.rows.windows(2).all(|w| w[0].score <= w[1].score));
    let lines = explain_lines(&core, "tenant-b", &sql);
    assert!(
        line(&lines, "ann_plan: ").starts_with("ann_plan: hnsw_subset"),
        "{lines:?}"
    );
}

/// fail-closed の維持: DESC・LIMIT 省略は厳密、precision 併用・`<->`・`<+>` は 0A000 のまま。
#[test]
fn unsupported_shapes_stay_fail_closed_with_ip_declaration() {
    let (core, _g, vb) = core_with_two_tenants("sql33-ip-failclosed", HnswScope::Declared);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    let q = vec_literal(&vb[0]);
    let desc = format!("SELECT id FROM docs ORDER BY embedding <#> '{q}' DESC LIMIT 5");
    let lines = explain_lines(&core, "tenant-b", &desc);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_descending"
    );
    let (_, consulted) = run(&core, "tenant-b", &desc);
    assert_eq!(consulted, 0);
    let omitted = format!("SELECT id FROM docs ORDER BY embedding <#> '{q}'");
    let lines = explain_lines(&core, "tenant-b", &omitted);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_limit_omitted"
    );
    assert_eq!(
        err_code(
            &core,
            "tenant-b",
            &format!(
                "SELECT id FROM docs ORDER BY embedding <#> '{q}' LIMIT 5 USING MODE 'precision'"
            )
        ),
        "0A000"
    );
    for op in ["<->", "<+>"] {
        assert_eq!(
            err_code(
                &core,
                "tenant-b",
                &format!("SELECT id FROM docs ORDER BY embedding {op} '{q}' LIMIT 5")
            ),
            "0A000",
            "{op}"
        );
    }
}

/// テナント境界: tenant-b の `<#>` の HNSW 結果に tenant-a の行は現れず、EXPLAIN にも情報が出ない。
#[test]
fn ip_hnsw_path_keeps_tenant_boundary() {
    let (core, _g, vb) = core_with_two_tenants("sql33-ip-rls", HnswScope::Declared);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    for i in 0..4 {
        let sql = knn_ip_sql(&vb[i * 11]);
        let (got, consulted) = run(&core, "tenant-b", &sql);
        assert!(consulted > 0);
        assert_eq!(got.rows.len(), 5);
        assert!(
            got.rows.iter().all(|r| r.id >= TENANT_B_ID_BASE),
            "{:?}",
            got.rows
        );
    }
    let lines = explain_lines(&core, "tenant-b", &knn_ip_sql(&vb[0]));
    for l in &lines {
        assert!(!l.contains("tenant"), "{l}");
        assert!(!l.contains(&HNSW_ROWS.to_string()), "{l}");
    }
}

/// 一致契約（Issue #3304）: HNSW 結果に含まれる行の距離値は厳密全走査とビット一致し、
/// 並びは距離昇順・同点は id 昇順になる。
#[test]
fn hnsw_distances_are_bit_identical_to_exact_scan() {
    let (baseline, _g0, _) = core_with_two_tenants("sql33-ip-bit-base", HnswScope::Declared);
    let (core, _g, vb) = core_with_two_tenants("sql33-ip-bit", HnswScope::Declared);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    for qi in 0..5 {
        let sql = format!(
            "SELECT id FROM docs ORDER BY embedding <#> '{}' LIMIT 20",
            vec_literal(&vb[qi * 13])
        );
        let (exact, _) = run(&baseline, "tenant-b", &sql);
        let (got, consulted) = run(&core, "tenant-b", &sql);
        assert!(consulted > 0);
        let exact_score: std::collections::BTreeMap<u64, u64> = exact
            .rows
            .iter()
            .map(|r| (r.id, r.score.to_bits()))
            .collect();
        for r in &got.rows {
            if let Some(bits) = exact_score.get(&r.id) {
                assert_eq!(r.score.to_bits(), *bits, "id {}", r.id);
            }
        }
        assert!(got
            .rows
            .windows(2)
            .all(|w| w[0].score < w[1].score || (w[0].score == w[1].score && w[0].id < w[1].id)));
    }
}

/// 全成分 0 のクエリは全行が同点のため全走査へ縮退し、宣言なしの厳密結果（id 昇順の先頭 k 件）と完全一致する。
#[test]
fn zero_query_matches_exact_scan() {
    let (baseline, _g0, _) = core_with_two_tenants("sql33-ip-zero-base", HnswScope::Declared);
    let (core, _g, _) = core_with_two_tenants("sql33-ip-zero", HnswScope::Declared);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    let sql = knn_ip_sql(&[0.0; DIM as usize]);
    let (exact, _) = run(&baseline, "tenant-b", &sql);
    let (got, consulted) = run(&core, "tenant-b", &sql);
    assert!(consulted > 0);
    assert_eq!(got, exact);
    assert!(core.hnsw_index_cache_stats().fallbacks > 0);
}

/// 内積がオーバーフローするクエリでも、宣言あり・なしで結果（またはエラーの SQLSTATE）が一致する。
#[test]
fn overflowing_query_matches_exact_scan() {
    let (baseline, _g0, _) = core_with_two_tenants("sql33-ip-ovf-base", HnswScope::Declared);
    let (core, _g, _) = core_with_two_tenants("sql33-ip-ovf", HnswScope::Declared);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    let sql = knn_ip_sql(&[3.0e38; DIM as usize]);
    let want = baseline.execute_sql(&ctx("tenant-b"), &sql);
    let got = core.execute_sql(&ctx("tenant-b"), &sql);
    match (want, got) {
        (Ok(w), Ok(g)) => assert_eq!(g, w),
        (Err(w), Err(g)) => assert_eq!(g.wire_code(), w.wire_code()),
        (w, g) => panic!("diverged: exact={w:?} hnsw={g:?}"),
    }
}

/// cosine と ip の両方を宣言した表: `index=` は宣言名の列挙（現行規則）、`<#>` は `metric=ip`、`<=>` は `metric=cosine`。
#[test]
fn explain_with_both_declarations_reports_query_metric() {
    let (core, _g, vb) = core_with_two_tenants("sql33-ip-both", HnswScope::Declared);
    ddl(&core, "CREATE INDEX idx_cos ON docs USING hnsw (embedding)");
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    let lines = explain_lines(&core, "tenant-b", &knn_ip_sql(&vb[0]));
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: hnsw_full_visible index=idx_cos,idx_ip"
    );
    assert!(
        line(&lines, "hnsw_params: ").contains("metric=ip,"),
        "{lines:?}"
    );
    let cos = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 5",
        vec_literal(&vb[0])
    );
    let lines = explain_lines(&core, "tenant-b", &cos);
    assert!(
        line(&lines, "hnsw_params: ").contains("metric=cosine,"),
        "{lines:?}"
    );
}
// ---------------------------------------------------------------------
// Issue #3305: Recall 下限・越境（カナリア）・再起動後の復元
// ---------------------------------------------------------------------

type Precision = engine::hnsw::ResidentPrecision;

const RECALL_DIM: u32 = 16;
// `MIN_INDEXED_ROWS` 超かつ並列構築の閾値（2,048）未満（構築が決定的）。`core_misc/hnsw_cache.rs` と同じ根拠。
const RECALL_ROWS: usize = 1_200;
const RECALL_K: usize = 10;
const RECALL_QUERIES: usize = 20;
/// クエリ抽出の間隔。コーパスは `i % 12` でクラスタ割当のため、12 と互いに素な値にして 12 クラスタすべてを網羅する
/// （`RECALL_QUERIES * RECALL_QUERY_STRIDE <= RECALL_ROWS` を満たす）。
const RECALL_QUERY_STRIDE: usize = 61;

/// 長さを正規化しない決定的なクラスタ付きコーパス。正規化済みだと内積とコサインの順位が一致し、
/// `<#>` 固有の検証にならないため、行ごとに長さを [0.25, 4] で変える。
fn gen_clustered_unnormalized(
    seed: u64,
    dim: usize,
    rows: usize,
    clusters: usize,
) -> Vec<Vec<f32>> {
    let rng = |state: &mut u64| -> f32 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        let bits = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    let mut cs = seed ^ 0xC1C1_C1C1_C1C1_C1C1;
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| rng(&mut cs)).collect())
        .collect();
    let mut st = seed;
    (0..rows)
        .map(|i| {
            let center = &centers[i % centers.len()];
            let scale = 0.25 + (rng(&mut st) + 1.0) * 1.875;
            center
                .iter()
                .map(|c| (c + rng(&mut st) * 0.2) * scale)
                .collect()
        })
        .collect()
}

/// `core_misc/hnsw_cache.rs::recall_at_k` の複製。
fn recall_at_k(got: &[u64], want: &[u64]) -> f64 {
    if want.is_empty() {
        return 1.0;
    }
    let want_set: std::collections::HashSet<u64> = want.iter().copied().collect();
    got.iter().filter(|id| want_set.contains(id)).count() as f64 / want.len() as f64
}

/// `core_misc/hnsw_cache.rs::min_recall_for` と同じ回帰基準（本リポの実装既定値で、spec 由来ではない）。
fn min_recall_for(precision: Precision) -> f64 {
    match precision {
        Precision::I8 => 0.8,
        Precision::F32 | Precision::F16 => 0.9,
    }
}

fn hnsw_kind_with(precision: Precision) -> search_engine::SearchEngineKind {
    match search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params")
    {
        search_engine::SearchEngineKind::Hnsw(v) => {
            search_engine::SearchEngineKind::Hnsw(v.with_resident_precision(precision))
        }
        other => panic!("hnsw_kind must return Hnsw, got {other:?}"),
    }
}

fn recall_core(
    label: &str,
    corpus: &[Vec<f32>],
    precision: Precision,
    declare_ip: bool,
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_dim(RECALL_DIM))
        .expect("create docs");
    seed_with(
        &storage,
        "tenant-a",
        0,
        Visibility::Public,
        corpus,
        RECALL_DIM,
        "recall",
    );
    let core = EngineCore::from_storage_with_engine(storage, hnsw_kind_with(precision))
        .with_hnsw_scope(HnswScope::Declared);
    if declare_ip {
        ddl(
            &core,
            "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
        );
    }
    (core, guard)
}

/// コサインと同じ手順（宣言なしの厳密側を対照にした Recall@10 の平均。クエリはコーパスから 12 と互いに素な間隔 61 で 20 点。全クラスタを対象にする）で、
/// `<#>` の HNSW の Recall が常駐精度別の下限（F32／F16 は 0.9、I8 は 0.8）以上であることを固定する。
/// 返る行の距離値は厳密側の同 id とビット一致する（一致契約）。
#[test]
fn ip_recall_at_10_meets_cosine_regression_floor_per_precision() {
    let corpus = gen_clustered_unnormalized(0x1B_3305, RECALL_DIM as usize, RECALL_ROWS, 12);
    for precision in [Precision::F32, Precision::F16, Precision::I8] {
        let (exact_core, _g0) = recall_core("sql33-ip-recall-exact", &corpus, precision, false);
        let (core, _g1) = recall_core("sql33-ip-recall-hnsw", &corpus, precision, true);
        let mut sum = 0.0f64;
        for qi in 0..RECALL_QUERIES {
            let q = &corpus[qi * RECALL_QUERY_STRIDE];
            let sql = format!(
                "SELECT id FROM docs ORDER BY embedding <#> '{}' LIMIT {RECALL_K}",
                vec_literal(q)
            );
            let (exact, consulted) = run(&exact_core, "tenant-a", &sql);
            assert_eq!(consulted, 0, "対照側は HNSW を照会しない");
            let (got, consulted) = run(&core, "tenant-a", &sql);
            assert!(consulted > 0, "{precision:?}: ip 宣言は HNSW へ届く");
            let exact_bits: std::collections::BTreeMap<u64, u64> = exact
                .rows
                .iter()
                .map(|r| (r.id, r.score.to_bits()))
                .collect();
            for r in &got.rows {
                if let Some(bits) = exact_bits.get(&r.id) {
                    assert_eq!(r.score.to_bits(), *bits, "{precision:?} id {}", r.id);
                }
            }
            let got_ids: Vec<u64> = got.rows.iter().map(|r| r.id).collect();
            let want_ids: Vec<u64> = exact.rows.iter().map(|r| r.id).collect();
            sum += recall_at_k(&got_ids, &want_ids);
        }
        let mean = sum / RECALL_QUERIES as f64;
        eprintln!("ip recall@{RECALL_K} {precision:?}: {mean:.4}");
        assert!(
            mean >= min_recall_for(precision),
            "{precision:?}: mean recall {mean} < {}",
            min_recall_for(precision)
        );
        let stats = core.hnsw_index_cache_stats();
        assert_eq!(stats.builds, 1, "{precision:?}: {stats:?}");
        assert!(stats.hits >= 1, "{precision:?}: {stats:?}");
        let kind = core
            .search_engine_kind()
            .map(|k| k.to_string())
            .unwrap_or_default();
        let want_suffix = match precision {
            Precision::F32 => "resident=f32",
            Precision::F16 => "resident=f16",
            Precision::I8 => "resident=i8",
        };
        assert!(kind.contains(want_suffix), "{kind}");
        assert_eq!(stats.f16_residency_fallbacks, 0, "{precision:?}");
        assert_eq!(stats.i8_residency_fallbacks, 0, "{precision:?}");
    }
}

/// 越境（カナリア）: tenant-a の Private 行に tenant-b のクエリの定数倍（内積が最大）のベクトルを置いても、
/// tenant-b の `<#>` の HNSW 結果には現れない。tenant-a で問い合わせると先頭に来る（検知が vacuous でない）。
/// キャッシュは ctx ごとに別エントリ（`(table, ctx, InnerProduct)`）になる。
#[test]
fn ip_hnsw_never_returns_other_tenant_canary_rows() {
    let q_b = gen_vectors(5, HNSW_ROWS)[0].clone();
    let canaries: Vec<Vec<f32>> = (0..30)
        .map(|i| q_b.iter().map(|x| x * (10.0 + i as f32 * 0.01)).collect())
        .collect();
    let (core, _g, _path, _vb) =
        core_with_two_tenants_at("sql33-ip-canary", HnswScope::Declared, &canaries);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    let sql = format!(
        "SELECT id FROM docs ORDER BY embedding <#> '{}' LIMIT 10",
        vec_literal(&q_b)
    );
    let is_canary = |id: u64| (CANARY_ID_BASE..CANARY_ID_BASE + 30).contains(&id);

    // tenant-a 側: カナリアが先頭に来る。
    let (as_a, consulted) = run(&core, "tenant-a", &sql);
    assert!(consulted > 0);
    assert!(is_canary(as_a.rows[0].id), "{:?}", as_a.rows);
    assert_eq!(core.hnsw_index_cache_stats().builds, 1);

    // tenant-b 側: HNSW を照会し、k 件満たし、全行が tenant-b の行。エントリは ctx ごとに別。
    let (as_b, consulted) = run(&core, "tenant-b", &sql);
    assert!(consulted > 0);
    assert_eq!(as_b.rows.len(), 10);
    assert!(
        as_b.rows.iter().all(|r| r.id >= TENANT_B_ID_BASE),
        "{:?}",
        as_b.rows
    );
    assert_eq!(core.hnsw_index_cache_stats().builds, 2);

    // 末尾 `;` の単独送信でも同じ。
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx("tenant-b"), &mut session, &format!("{sql};"))
        .expect("session query")
    {
        SqlOutcome::Query(r) => assert_eq!(r, as_b),
        other => panic!("expected query outcome, got {other:?}"),
    }
    let lines = explain_lines(&core, "tenant-b", &sql);
    for l in &lines {
        assert!(!l.contains("tenant"), "{l}");
        assert!(!l.contains(&HNSW_ROWS.to_string()), "{l}");
    }
}

/// 再起動後の `<#>` の共通検査: HNSW を照会し、k 件返り、距離値は `-dot`。
fn assert_ip_hnsw_result(core: &EngineCore, vb: &[Vec<f32>], q: &[f32], k: usize) -> QueryResult {
    let sql = format!(
        "SELECT id FROM docs ORDER BY embedding <#> '{}' LIMIT {k}",
        vec_literal(q)
    );
    let (got, consulted) = run(core, "tenant-b", &sql);
    assert!(consulted > 0, "restart must restore the ip route");
    assert_eq!(got.rows.len(), k);
    for r in &got.rows {
        assert!(r.id >= TENANT_B_ID_BASE);
        let v = &vb[(r.id - TENANT_B_ID_BASE) as usize];
        assert!((r.score + dot(q, v)).abs() < 1e-4, "score must be -dot");
    }
    got
}

/// 再起動（`Storage` の再オープン）後も、永続カタログの宣言 opclass からゲート・写像・キャッシュキーが
/// 復元され、`<#>` は HNSW を使う。fail-closed の形も保たれる。
#[test]
fn restart_restores_ip_declaration_and_hnsw_route() {
    let (core, _g, path, vb) =
        core_with_two_tenants_at("sql33-ip-restart", HnswScope::Declared, &[]);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    let q = vb[17].clone();
    let before = assert_ip_hnsw_result(&core, &vb, &q, 10);
    drop(core);

    let core = reopen(&path, HnswScope::Declared);
    let sql = format!(
        "SELECT id FROM docs ORDER BY embedding <#> '{}' LIMIT 10",
        vec_literal(&q)
    );
    let lines = explain_lines(&core, "tenant-b", &sql);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: hnsw_full_visible index=idx_ip"
    );
    assert!(
        line(&lines, "hnsw_params: ").contains("metric=ip,"),
        "{lines:?}"
    );
    let after = assert_ip_hnsw_result(&core, &vb, &q, 10);
    assert_eq!(after, before, "構築は決定的で再起動前後の結果は一致する");
    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.builds, 1,
        "キャッシュはメモリ上のみ: 再起動後は作り直し"
    );
    let hits_before = stats.hits;
    assert_ip_hnsw_result(&core, &vb, &q, 10);
    assert!(core.hnsw_index_cache_stats().hits > hits_before);

    // 末尾 `;` の単独送信。
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx("tenant-b"), &mut session, &format!("{sql};"))
        .expect("session query")
    {
        SqlOutcome::Query(r) => assert_eq!(r, before),
        other => panic!("expected query outcome, got {other:?}"),
    }

    // ip だけの宣言なので `<=>` は厳密（HNSW 非照会）。
    let cos = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 5",
        vec_literal(&q)
    );
    let (_, consulted) = run(&core, "tenant-b", &cos);
    assert_eq!(consulted, 0);
    let lines = explain_lines(&core, "tenant-b", &cos);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_opclass_mismatch"
    );

    // fail-closed の維持。
    let ql = vec_literal(&q);
    assert_eq!(
        err_code(
            &core,
            "tenant-b",
            &format!(
                "SELECT id FROM docs ORDER BY embedding <#> '{ql}' LIMIT 5 USING MODE 'precision'"
            )
        ),
        "0A000"
    );
    for op in ["<->", "<+>"] {
        assert_eq!(
            err_code(
                &core,
                "tenant-b",
                &format!("SELECT id FROM docs ORDER BY embedding {op} '{ql}' LIMIT 5")
            ),
            "0A000",
            "{op}"
        );
    }
    let desc = format!("SELECT id FROM docs ORDER BY embedding <#> '{ql}' DESC LIMIT 5");
    let lines = explain_lines(&core, "tenant-b", &desc);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_descending"
    );
}

/// cosine と ip の両宣言を再起動しても、`<#>` と `<=>` は物理グラフ（`(table, ctx, InnerProduct)`）を共有する。
#[test]
fn restart_with_cos_and_ip_shares_inner_product_cache_key() {
    let (core, _g, path, vb) =
        core_with_two_tenants_at("sql33-ip-restart-both", HnswScope::Declared, &[]);
    ddl(&core, "CREATE INDEX idx_cos ON docs USING hnsw (embedding)");
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    drop(core);

    let core = reopen(&path, HnswScope::Declared);
    let q = vb[5].clone();
    assert_ip_hnsw_result(&core, &vb, &q, 10);
    let s1 = core.hnsw_index_cache_stats();
    assert_eq!(s1.builds, 1);
    let cos = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 10",
        vec_literal(&q)
    );
    let (got, consulted) = run(&core, "tenant-b", &cos);
    assert!(consulted > 0);
    assert_eq!(got.rows.len(), 10);
    assert!(got.rows.iter().all(|r| r.id >= TENANT_B_ID_BASE));
    let s2 = core.hnsw_index_cache_stats();
    assert_eq!(s2.builds, 1, "物理グラフは共有され再構築しない: {s2:?}");
    assert!(s2.hits > s1.hits, "{s1:?} -> {s2:?}");
    let lines = explain_lines(&core, "tenant-b", &knn_ip_sql(&q));
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: hnsw_full_visible index=idx_cos,idx_ip"
    );
    assert!(line(&lines, "hnsw_params: ").contains("metric=ip,"));
    let lines = explain_lines(&core, "tenant-b", &cos);
    assert!(line(&lines, "hnsw_params: ").contains("metric=cosine,"));
}

/// ip 宣言を DROP してから再起動すると、宣言が 0 件なので `<#>` は厳密全走査（`plain_scan_engine`）で宣言なしと同じ結果になる。
#[test]
fn restart_after_drop_index_keeps_exact_scan() {
    let (baseline, _g0, _p0, _) =
        core_with_two_tenants_at("sql33-ip-restart-drop-base", HnswScope::Declared, &[]);
    let (core, _g, path, vb) =
        core_with_two_tenants_at("sql33-ip-restart-drop", HnswScope::Declared, &[]);
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    ddl(&core, "DROP INDEX idx_ip");
    drop(core);

    let core = reopen(&path, HnswScope::Declared);
    let sql = knn_ip_sql(&vb[2]);
    let (got, consulted) = run(&core, "tenant-b", &sql);
    assert_eq!(consulted, 0);
    let lines = explain_lines(&core, "tenant-b", &sql);
    assert_eq!(line(&lines, "ann_plan: "), "ann_plan: plain_scan_engine");
    let (exact, _) = run(&baseline, "tenant-b", &sql);
    assert_eq!(got, exact);
}

/// `HnswScope::All` でも再起動後に ip 宣言があれば `<#>` は HNSW を照会し、宣言なしなら厳密のまま。
#[test]
fn restart_scope_all_with_ip_declaration_routes_hnsw() {
    let (core, _g, path, vb) =
        core_with_two_tenants_at("sql33-ip-restart-all", HnswScope::All, &[]);
    let (_, consulted) = run(&core, "tenant-b", &knn_ip_sql(&vb[1]));
    assert_eq!(consulted, 0, "宣言なしの All は <#> を HNSW へ送らない");
    ddl(
        &core,
        "CREATE INDEX idx_ip ON docs USING hnsw (embedding vector_ip_ops)",
    );
    drop(core);

    let core = reopen(&path, HnswScope::All);
    assert_ip_hnsw_result(&core, &vb, &vb[1], 10);
    let lines = explain_lines(&core, "tenant-b", &knn_ip_sql(&vb[1]));
    assert!(
        line(&lines, "hnsw_params: ").contains("metric=ip,"),
        "{lines:?}"
    );
}
