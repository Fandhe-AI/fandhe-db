//! `<=>`（コサイン距離）と `<#>`（負の内積）の実行の受け入れテスト（Issue #1991。ビヘイビア:
//! SQL-33 (1)(2)(3)(7)・SQL-34・SQL-9。ポインタ: `docs/spec/04-behavior/sql-surface.md`）。
//!
//! 固定する契約:
//! - `<=>` は `1 - cos`（f64）の昇順で、同点は行の順序キー昇順。ゼロノルム行の距離は NaN で
//!   昇順の末尾（`DESC` では先頭）に置き、`LIMIT` の枠を消費する。ゼロノルムのクエリでは全行が
//!   NaN になり、順序キー昇順の先頭 `k` 行が返る。
//! - `<#>` は負の内積の昇順（内積の降順）。順位は #1991 以前の `<=>` と同じ。
//! - `ResultRow.score` は演算子の距離値（`<=>` は `1 - cos`、`<#>` は `-dot`）。
//! - `WHERE`（事前・事後フィルタ、スカラー索引の候補マスクの hot 経路）、`DESC`、`OFFSET`、
//!   `LIMIT` 省略でも全件の厳密参照と一致する。HYBRID の密のみ縮退経路の密側もコサイン順。
//! - 他テナントの行（コサインでより近い行・ゼロノルム行を含む）は結果にも応答の形にも影響しない。
//! - `precision` と `<#>` の併用は `0A000`（Execute・EXPLAIN・Describe で一致）、`<=>` は成功する。
//!   `<->`／`<+>` は従来どおり `0A000`。
//!
//! フィクスチャは内積順とコサイン順が食い違うよう、ノルムの異なる向きを混ぜた固定値にしてある
//! （期待値の手計算は各テストのコメント）。クエリは `[1,0]`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// (id, embedding, kind)。クエリ `[1,0]` に対する値:
///
/// | id | embedding | dot | cos 距離（1 - cos）|
/// |----|-----------|-----|---------------------|
/// | 1  | [10,10]   | 10  | 1 - 1/√2 ≈ 0.2929   |
/// | 2  | [1,0]     | 1   | 0                   |
/// | 3  | [0,5]     | 0   | 1                   |
/// | 4  | [2,0]     | 2   | 0（id 2 と同点）    |
/// | 5  | [0,1]     | 0   | 1（id 3 と同点）    |
/// | 6  | [0,0]     | 0   | NaN（ゼロノルム）   |
///
/// コサイン昇順: 2, 4, 1, 3, 5, 6(NaN)。内積降順（`<#>` 昇順）: 1, 4, 2, 3, 5, 6（dot 0 の同点は id 昇順）。
fn rows() -> Vec<(u64, [f32; 2], &'static str)> {
    vec![
        (1, [10.0, 10.0], "a"),
        (2, [1.0, 0.0], "b"),
        (3, [0.0, 5.0], "a"),
        (4, [2.0, 0.0], "b"),
        (5, [0.0, 1.0], "a"),
        (6, [0.0, 0.0], "b"),
    ]
}

const COSINE_ASC: [u64; 6] = [2, 4, 1, 3, 5, 6];
const NEGIP_ASC: [u64; 6] = [1, 4, 2, 3, 5, 6];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("kind", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
}

fn seed_tenant(core: &EngineCore, tenant: &str, rows: &[(u64, [f32; 2], &str)]) {
    let ctx = ctx(tenant);
    let mut session = SessionState::default();
    for (id, v, kind) in rows {
        let sql = format!(
            "INSERT INTO docs (id, embedding, kind) VALUES ({id}, '[{},{}]', '{kind}') \
             USING OPERATION_ID 'seed-{tenant}-{id}'",
            v[0], v[1]
        );
        core.execute_sql_in_session(&ctx, &mut session, &sql)
            .unwrap_or_else(|e| panic!("seed {sql}: {e:?}"));
    }
}

fn seeded(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    seed_tenant(&core, "tenant-a", &rows());
    (core, guard)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    {
        SqlOutcome::Query(result) => result,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn err_code(core: &EngineCore, tenant: &str, sql: &str) -> (String, String) {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql) {
        Err(e) => (e.wire_code().to_string(), e.to_string()),
        Ok(_) => panic!("{sql} must fail"),
    }
}

fn ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

fn knn(core: &EngineCore, op: &str, tail: &str) -> Vec<u64> {
    ids(&run(
        core,
        "tenant-a",
        &format!("SELECT id FROM docs ORDER BY embedding {op} '[1,0]' {tail}"),
    ))
}

// SQL-33 (1): `<=>` はコサイン距離の昇順（同点は id 昇順）。内積順とは食い違う。
#[test]
fn sql33_cosine_orders_by_cosine_distance_not_inner_product() {
    let (core, _g) = seeded("sql33-cos-order");
    assert_eq!(knn(&core, "<=>", "LIMIT 6"), COSINE_ASC.to_vec());
    // 内積順（1, 4, 2, ...）とは先頭から異なる。
    assert_ne!(knn(&core, "<=>", "LIMIT 6"), NEGIP_ASC.to_vec());
    // ゼロノルム行を除いた先頭 5 件。
    assert_eq!(knn(&core, "<=>", "LIMIT 5"), vec![2, 4, 1, 3, 5]);
    // 同点（id 2 と 4）は k の境界でも id 昇順の先頭が残る。
    assert_eq!(knn(&core, "<=>", "LIMIT 1"), vec![2]);
    assert_eq!(knn(&core, "<=>", "LIMIT 2"), vec![2, 4]);
}

// SQL-33 (2): `<#>` は負の内積の昇順＝旧 `<=>` と同じ順位。同点は id 昇順。
#[test]
fn sql33_negative_inner_product_matches_legacy_inner_product_order() {
    let (core, _g) = seeded("sql33-negip-order");
    assert_eq!(knn(&core, "<#>", "LIMIT 6"), NEGIP_ASC.to_vec());
    // dot 0 の同点（3, 5, 6）は id 昇順で、k の境界で切れる。
    assert_eq!(knn(&core, "<#>", "LIMIT 4"), vec![1, 4, 2, 3]);
}

// SQL-33 (3): ゼロノルム行は NaN で昇順の末尾・DESC の先頭。NaN 行も k の枠を消費する。
#[test]
fn sql33_zero_norm_row_is_nan_last_in_asc_first_in_desc_and_consumes_a_slot() {
    let (core, _g) = seeded("sql33-cos-nan");
    assert_eq!(knn(&core, "<=>", "LIMIT 6").last(), Some(&6));
    // DESC: NaN(6) → 距離 1 の同点(3, 5) → 0.2929(1) → 距離 0 の同点(2, 4)。
    assert_eq!(knn(&core, "<=>", "DESC LIMIT 6"), vec![6, 3, 5, 1, 2, 4]);
    assert_eq!(knn(&core, "<=>", "DESC LIMIT 2"), vec![6, 3]);
    let rows = run(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' DESC LIMIT 1",
    )
    .rows;
    assert!(rows[0].score.is_nan());
}

// SQL-33 (3): ゼロノルムのクエリでは全行が NaN になり、id 昇順の先頭 k 行が返る。
#[test]
fn sql33_zero_norm_query_returns_first_k_rows_by_id() {
    let (core, _g) = seeded("sql33-cos-zero-query");
    let result = run(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY embedding <=> '[0,0]' LIMIT 3",
    );
    assert_eq!(ids(&result), vec![1, 2, 3]);
    assert!(result.rows.iter().all(|r| r.score.is_nan()));
}

// SQL-33 (2)(8): DESC・OFFSET・LIMIT 省略は両演算子で全件の厳密参照と一致する。
#[test]
fn sql33_desc_offset_and_limit_omitted_for_both_operators() {
    let (core, _g) = seeded("sql33-cos-desc-offset");
    // `<=>`
    assert_eq!(knn(&core, "<=>", ""), COSINE_ASC.to_vec());
    assert_eq!(knn(&core, "<=>", "LIMIT 2 OFFSET 2"), vec![1, 3]);
    assert_eq!(knn(&core, "<=>", "OFFSET 4"), vec![5, 6]);
    assert_eq!(knn(&core, "<=>", "DESC"), vec![6, 3, 5, 1, 2, 4]);
    assert_eq!(knn(&core, "<=>", "DESC LIMIT 2 OFFSET 1"), vec![3, 5]);
    // `<#>`: 遠い順は dot の昇順（0 の同点 3, 5, 6 → 1 → 2 → 10）。
    assert_eq!(knn(&core, "<#>", ""), NEGIP_ASC.to_vec());
    assert_eq!(knn(&core, "<#>", "DESC"), vec![3, 5, 6, 2, 4, 1]);
    assert_eq!(knn(&core, "<#>", "LIMIT 2 OFFSET 2"), vec![2, 3]);
    assert_eq!(knn(&core, "<#>", "DESC LIMIT 2 OFFSET 1"), vec![5, 6]);
}

// SQL-33 (8): 結果の `score` は演算子の距離値（`<=>` は 1 - cos、`<#>` は -dot）。
#[test]
fn sql33_result_score_is_the_operator_distance_value() {
    let (core, _g) = seeded("sql33-cos-score");
    let cos = run(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 3",
    );
    let want = [(2, 0.0), (4, 0.0), (1, 1.0 - 1.0 / 2.0_f64.sqrt())];
    for (row, (id, d)) in cos.rows.iter().zip(want) {
        assert_eq!(row.id, id);
        assert!((row.score - d).abs() < 1e-9, "id={id} score={}", row.score);
    }
    let neg = run(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY embedding <#> '[1,0]' LIMIT 3",
    );
    let want = [(1, -10.0), (4, -2.0), (2, -1.0)];
    for (row, (id, d)) in neg.rows.iter().zip(want) {
        assert_eq!(row.id, id);
        assert_eq!(row.score, d, "id={id}");
    }
}

// WHERE の事前フィルタ・事後フィルタ（HINT ORDER）・スカラー索引の候補マスクの hot 経路でも、
// 全件の厳密参照（フィルタ後の同じ順序）と一致する。
#[test]
fn sql33_where_paths_match_exact_reference_for_both_operators() {
    let (core, _g) = seeded("sql33-cos-where");
    // kind = 'a' の行: 1, 3, 5。コサイン昇順は 1, 3, 5。内積降順は 1, 3, 5（dot 10, 0, 0）。
    // kind = 'b' の行: 2, 4, 6。コサイン昇順は 2, 4, 6(NaN)。内積降順は 4, 2, 6。
    let cases = [
        ("<=>", "a", vec![1, 3, 5]),
        ("<=>", "b", vec![2, 4, 6]),
        ("<#>", "a", vec![1, 3, 5]),
        ("<#>", "b", vec![4, 2, 6]),
    ];
    for (op, kind, want) in cases {
        let sql = format!(
            "SELECT id FROM docs WHERE kind = '{kind}' ORDER BY embedding {op} '[1,0]' LIMIT 10"
        );
        // 1 回目は cold（redb 走査）、2 回目以降は hot（スカラー索引の候補マスク経路）。
        for round in 0..3 {
            assert_eq!(
                ids(&run(&core, "tenant-a", &sql)),
                want,
                "{op} {kind} round={round}"
            );
        }
        // 事後フィルタ（DISTANCE 先行）: 全可視行の上位 k 件から絞る。k が全件以上なら同じ結果。
        let hinted = format!("{sql} HINT ORDER(DISTANCE, SCALAR, RLS)");
        assert_eq!(
            ids(&run(&core, "tenant-a", &hinted)),
            want,
            "{op} {kind} hinted"
        );
    }
}

// HYBRID の密のみ縮退経路（疎側が空）の密側は `<=>` の意味（コサイン）で順位付けされる。
// ゼロノルム行（NaN）は密側の候補から外れる。
#[test]
fn sql33_hybrid_dense_only_degradation_ranks_dense_side_by_cosine() {
    let path = unique_db_path("sql33-cos-hybrid");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    // body を一切持たない行だけにして疎側のコーパスを空にする（密のみ縮退）。
    seed_tenant(&core, "tenant-a", &rows());
    let result = run(
        &core,
        "tenant-a",
        "SELECT id FROM docs ORDER BY HYBRID(embedding, '[1,0]', body, 'anything') LIMIT 10",
    );
    // コサイン昇順（2 と 4、3 と 5 は融合後も同順位の同点で id 昇順）。内積順なら先頭は 1。
    assert_eq!(ids(&result), vec![2, 4, 1, 3, 5]);
}

// RLS: 他テナントの行（コサインでより近い行・ゼロノルム行）は結果・件数・応答の形に影響しない。
#[test]
fn sql33_other_tenants_rows_never_affect_results_or_response_shape() {
    let (core, _g) = seeded("sql33-cos-rls");
    seed_tenant(
        &core,
        "tenant-b",
        &[
            (101, [1.0, 0.0], "a"),
            (102, [3.0, 0.0], "b"),
            (103, [0.0, 0.0], "a"),
        ],
    );
    for (op, want) in [("<=>", COSINE_ASC.to_vec()), ("<#>", NEGIP_ASC.to_vec())] {
        for tail in ["LIMIT 6", "LIMIT 100", "DESC LIMIT 100", ""] {
            let a = knn(&core, op, tail);
            assert!(a.iter().all(|id| *id < 100), "{op} {tail}: {a:?}");
            assert_eq!(a.len(), 6, "{op} {tail}");
        }
        assert_eq!(knn(&core, op, "LIMIT 6"), want, "{op}");
        // 他テナントだけが行を持たないテナントでも、応答の形（列・件数 0）は同じ。
        let none = run(
            &core,
            "tenant-none",
            &format!("SELECT id FROM docs ORDER BY embedding {op} '[1,0]' LIMIT 5"),
        );
        assert!(none.rows.is_empty(), "{op}");
        let b = run(
            &core,
            "tenant-b",
            &format!("SELECT id FROM docs ORDER BY embedding {op} '[1,0]' LIMIT 5"),
        );
        assert_eq!(none.columns, b.columns, "{op}");
        assert_eq!(b.rows.len(), 3, "{op}");
    }
}

// precision との併用: `<#>` は 0A000（Execute・EXPLAIN・Describe で一致）、`<=>` は成功する。
#[test]
fn sql33_precision_with_negative_inner_product_is_rejected_consistently() {
    let (core, _g) = seeded("sql33-negip-precision");
    let base = "ORDER BY embedding <#> '[1,0]' LIMIT 3 USING MODE 'precision'";
    let exec = err_code(&core, "tenant-a", &format!("SELECT id FROM docs {base}"));
    let explain = err_code(
        &core,
        "tenant-a",
        &format!("EXPLAIN SELECT id FROM docs {base}"),
    );
    assert_eq!(exec.0, "0A000");
    assert_eq!(explain.0, "0A000");
    assert_eq!(exec, explain);
    assert!(!exec.1.contains("docs"), "{}", exec.1);
    // Describe（拡張クエリプロトコル）も同じ判定。
    let prepared = core
        .parse_sql_prepared(
            "SELECT id FROM docs ORDER BY embedding <#> $1 LIMIT 3 USING MODE 'precision'",
        )
        .expect("parse_sql_prepared");
    let described = core.describe_prepared_in_session(&SessionState::default(), &prepared);
    assert_eq!(
        described.expect_err("describe must fail").wire_code(),
        "0A000"
    );
    // `<=>` と precision は成功する（確信度は cosine 類似度で判定するため順位付けと整合する）。
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 3 USING MODE 'precision'",
    )
    .expect("cosine + precision must succeed");
}

// `<->`／`<+>` は SQL 実行経路に未接続のまま 0A000（fail-closed）。
#[test]
fn sql33_l2_and_l1_remain_feature_not_supported() {
    let (core, _g) = seeded("sql33-unconnected");
    for op in ["<->", "<+>"] {
        let (code, msg) = err_code(
            &core,
            "tenant-a",
            &format!("SELECT id FROM docs ORDER BY embedding {op} '[1,0]' LIMIT 3"),
        );
        assert_eq!(code, "0A000", "{op}");
        assert!(!msg.contains("docs"), "{op}: {msg}");
    }
}
