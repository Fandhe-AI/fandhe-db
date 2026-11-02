//! 利用者ポリシーの `USING` を kNN・ハイブリッド検索・索引走査の経路へ適用する（RLS-13・Issue #2279）の
//! SQL 水準の結合テスト。ポインタ: `docs/spec/04-behavior/rls.md` の RLS-13、設計記録は
//! `docs/design/row-security-policy-composition.md` の「検索・索引走査への適用（#2279）」節。
//!
//! 固定する契約（期待値はいずれもテスト側の独立オラクル。production の判定関数は使わない）:
//! 1. ポリシーで除かれた行は、検索結果・順位・スコアのどれにも現れない。除外行が上位 k の枠を消費せず、
//!    候補が足りる限り `LIMIT k` 件を返す（距離順・`HINT ORDER`・`WHERE` との合成）
//! 2. 隠れた行にだけ評価エラーを起こす式が利用者の `WHERE` に混じっても、ポリシーが先に行を除く
//! 3. ハイブリッドの疎側（BM25）の統計も隠れた行に依存しない（隠れた行を最初から入れていない兄弟表と一致）
//! 4. HNSW（opt-in）でも `LIMIT k` 件を満たし、隠れた行は結果に出ない。ANN 経路を実際に通る（空振りでない）
//! 5. `row_security = off` は `42501`、他テナントの有効化・定義は効かない、DISABLE 後は従来の結果に戻る
//! 6. `EXPLAIN` は適用表で `FullVisible` を報告しない

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("n", ColumnType::Integer, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// シード行の真実（オラクル用）。ベクトルは角度 `0.05 * id` の単位ベクトル（クエリ `[1,0]` からの角度が
/// 小さいほど近い。隣り合う id の cosine 距離差が十分大きく、同点にならない）。
#[derive(Clone)]
struct Truth {
    id: u64,
    tenant: &'static str,
    vis: Visibility,
    lang: &'static str,
    n: i32,
    body: String,
}

fn vector_of(id: u64, step: f32) -> Vec<f32> {
    // bob の Private 行（id 200 以上）はクエリ `[1,0]` の極めて近くへ置く（漏れれば先頭に出る）。
    let theta = if id >= 200 {
        (id - 200) as f32 * 0.0005
    } else {
        id as f32 * step
    };
    vec![theta.cos(), theta.sin()]
}

fn insert(storage: &Storage, table: &str, t: &Truth, step: f32) {
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(t.tenant),
        t.id,
        t.vis,
        &[
            Value::Vector(vector_of(t.id, step)),
            Value::Text(t.lang.to_string()),
            Value::Integer(t.n),
            Value::Text(t.body.clone()),
        ],
        &OperationId::parse(&format!("seed-{table}-{}", t.id)).expect("op id"),
    )
    .expect("seed row");
}

/// alice の Public 行 1..=40。id 1..=10 は `lang = 'hide'`（クエリに最も近い行）、残りは `'keep'`。
/// id 1 は `n = 0` の 0 除算カナリア（隠れた行にだけ置く）。bob の Private 行（id 201..=203）は
/// クエリに極めて近い位置に置く（漏れれば先頭に出る）。
fn rows() -> Vec<Truth> {
    let mut v = Vec::new();
    for id in 1..=40u64 {
        let hidden = id <= 10;
        v.push(Truth {
            id,
            tenant: "alice",
            vis: Visibility::Public,
            lang: if hidden { "hide" } else { "keep" },
            n: if id == 1 { 0 } else { id as i32 },
            body: if hidden {
                // 隠れた文書はクエリ語を多く含む（BM25 の統計へ漏れれば順位・スコアが変わる）。
                "alpha alpha alpha alpha beta".to_string()
            } else if id % 2 == 0 {
                "alpha gamma".to_string()
            } else {
                "gamma delta".to_string()
            },
        });
    }
    for id in 201..=203u64 {
        v.push(Truth {
            id,
            tenant: "bob",
            vis: Visibility::Private,
            lang: "keep",
            n: 7,
            body: "alpha alpha alpha".to_string(),
        });
    }
    v
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema("docs")).expect("create docs");
    // 兄弟表: ポリシーを通る alice の行だけを最初から入れた表（BM25 統計の比較用）。
    storage
        .create_table(&schema("docs2"))
        .expect("create docs2");
    for t in rows() {
        insert(&storage, "docs", &t, 0.05);
        // bob の Private 行は alice に常に不可視なので、兄弟表にも入れない（可視集合を揃える）。
        if t.tenant == "alice" && t.lang == "keep" {
            insert(&storage, "docs2", &t, 0.05);
        }
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn query_as(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    match ok_as(core, tenant, sql) {
        SqlOutcome::Query(q) => q,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn ranked_ids(core: &EngineCore, tenant: &str, sql: &str) -> Vec<u64> {
    query_as(core, tenant, sql)
        .rows
        .iter()
        .map(|r| r.id)
        .collect()
}

fn enable_keep_policy(core: &EngineCore, table: &str) {
    ok_as(
        core,
        "alice",
        &format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY"),
    );
    ok_as(
        core,
        "alice",
        &format!("CREATE POLICY pk ON {table} AS PERMISSIVE FOR ALL USING (lang = 'keep')"),
    );
}

/// 独立オラクル: alice に見え、ポリシー（`lang = 'keep'`）を通る行を、クエリ `[1,0]` に近い順に並べた id。
/// id が大きいほど角度が大きく遠い（角度 `0.05 * id` は 40 * 0.05 = 2.0 rad < pi で単調）。
fn oracle_knn(limit: usize, extra: impl Fn(&Truth) -> bool) -> Vec<u64> {
    let mut v: Vec<&Truth> = Vec::new();
    let all = rows();
    for t in &all {
        if (t.tenant == "alice" || matches!(t.vis, Visibility::Public))
            && t.lang == "keep"
            && extra(t)
        {
            v.push(t);
        }
    }
    let mut ids: Vec<u64> = v.iter().map(|t| t.id).collect();
    ids.sort_unstable();
    ids.truncate(limit);
    ids
}

// ---------------------------------------------------------------------
// 1. 厳密 kNN
// ---------------------------------------------------------------------

/// 最も近い行をポリシーで隠したとき、除外行が枠を消費せず `LIMIT k` 件を返し、隠れた id は出ない。
#[test]
fn exact_knn_returns_k_policy_passing_rows_in_oracle_order() {
    let (core, _g) = open_core("rls13-search-knn");
    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 5";
    let before = ranked_ids(&core, "alice", sql);
    assert_eq!(before, vec![1, 2, 3, 4, 5], "ポリシー前は最も近い行が返る");
    enable_keep_policy(&core, "docs");
    let after = ranked_ids(&core, "alice", sql);
    assert_eq!(after, oracle_knn(5, |_| true));
    assert_eq!(after.len(), 5);
    // 繰り返し（キャッシュ経路）でも同じ。
    assert_eq!(ranked_ids(&core, "alice", sql), after);
    // 全件を要求しても、隠れた行・他テナントの Private 行は出ない。
    let all = ranked_ids(
        &core,
        "alice",
        "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 100",
    );
    assert_eq!(all, oracle_knn(100, |_| true));
    assert!(all.iter().all(|id| *id > 10 && *id < 201));
}

/// `HINT ORDER`（DISTANCE 先行）でもポリシーはランキングより前に効く。
#[test]
fn distance_first_order_still_applies_policy_before_ranking() {
    let (core, _g) = open_core("rls13-search-distance-first");
    enable_keep_policy(&core, "docs");
    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 5 \
               HINT ORDER(DISTANCE, SCALAR, RLS)";
    assert_eq!(ranked_ids(&core, "alice", sql), oracle_knn(5, |_| true));
}

/// ポリシー ∧ 利用者の `WHERE`。隠れた行にだけ 0 除算を起こす式（`n = 0` は id 1 のみで隠れている）を
/// 利用者の式に置いても、ポリシーが先に評価されるためエラーにならない。
#[test]
fn policy_composes_with_where_and_runs_before_user_expressions() {
    let (core, _g) = open_core("rls13-search-where");
    enable_keep_policy(&core, "docs");
    let sql = "SELECT id FROM docs WHERE 100 / n > 0 ORDER BY embedding <=> '[1,0]' LIMIT 5";
    assert_eq!(ranked_ids(&core, "alice", sql), oracle_knn(5, |_| true));
    let sql_or = "SELECT id FROM docs WHERE n = 12 OR n = 14 \
                  ORDER BY embedding <=> '[1,0]' LIMIT 5";
    assert_eq!(
        ranked_ids(&core, "alice", sql_or),
        oracle_knn(5, |t| t.n == 12 || t.n == 14)
    );
    let sql_decl = "SELECT id FROM docs WHERE lang = 'hide' ORDER BY embedding <=> '[1,0]' LIMIT 5";
    assert!(ranked_ids(&core, "alice", sql_decl).is_empty());
}

// ---------------------------------------------------------------------
// 3. ハイブリッド
// ---------------------------------------------------------------------

/// 隠れた文書がクエリ語を多く含んでも、ハイブリッドのスコア・順位は「隠れた文書を最初から入れていない
/// 兄弟表」と完全に一致する（BM25 の文書数・df・平均長が隠れた行に依存しない）。
#[test]
fn hybrid_scores_and_order_do_not_depend_on_hidden_rows() {
    let (core, _g) = open_core("rls13-search-hybrid");
    enable_keep_policy(&core, "docs");
    for limit in [3usize, 5, 20] {
        let q = |table: &str| {
            format!(
                "SELECT id FROM {table} ORDER BY HYBRID(embedding, '[1,0]', body, 'alpha') \
                 LIMIT {limit}"
            )
        };
        let guarded = query_as(&core, "alice", &q("docs"));
        let sibling = query_as(&core, "alice", &q("docs2"));
        let g: Vec<(u64, f64)> = guarded.rows.iter().map(|r| (r.id, r.score)).collect();
        let s: Vec<(u64, f64)> = sibling.rows.iter().map(|r| (r.id, r.score)).collect();
        assert_eq!(g, s, "limit={limit}");
        assert!(g.iter().all(|(id, _)| *id > 10 && *id < 201));
    }
}

// ---------------------------------------------------------------------
// 5. row_security・テナント境界
// ---------------------------------------------------------------------

/// 適用表への検索は `row_security = off` で `42501`（`0A000` ではない）。他テナント・DISABLE 後は従来どおり。
#[test]
fn row_security_off_is_rejected_and_other_tenants_and_disable_are_unaffected() {
    let (core, _g) = open_core("rls13-search-off");
    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 3";
    enable_keep_policy(&core, "docs");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, "SET row_security = off")
        .expect("set");
    let e = core
        .execute_sql_in_session(&ctx("alice"), &mut session, sql)
        .expect_err("must be rejected");
    assert_eq!(e.wire_code(), "42501");
    // 他テナント（bob）は有効化の影響を受けない。bob の Private 行は bob 自身にのみ見える。
    let bob = ranked_ids(&core, "bob", sql);
    assert_eq!(bob.len(), 3);
    // DISABLE 後は従来の結果（隠れていた行も返る）。
    ok_as(
        &core,
        "alice",
        "ALTER TABLE docs DISABLE ROW LEVEL SECURITY",
    );
    assert_eq!(ranked_ids(&core, "alice", sql), vec![1, 2, 3]);
}

// ---------------------------------------------------------------------
// 6. EXPLAIN
// ---------------------------------------------------------------------

fn explain_lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    match ok_as(core, tenant, sql) {
        SqlOutcome::Explain(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected text cell, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn hnsw_core(label: &str, rows_n: u64, hide_upto: u64) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema("docs")).expect("create docs");
    for id in 1..=rows_n {
        insert(
            &storage,
            "docs",
            &Truth {
                id,
                tenant: "alice",
                vis: Visibility::Public,
                lang: if id <= hide_upto { "hide" } else { "keep" },
                n: id as i32,
                body: if id % 3 == 0 {
                    "alpha gamma".to_string()
                } else {
                    "gamma delta".to_string()
                },
            },
            0.0013,
        );
    }
    let validated = engine::hnsw::ValidatedHnswParams::new(engine::hnsw::HnswParams::default())
        .expect("valid hnsw params");
    let kind = engine::search_engine::SearchEngineKind::Hnsw(validated);
    (EngineCore::from_storage_with_engine(storage, kind), guard)
}

/// HNSW opt-in の適用表では `ann_plan:` が `FullVisible` にならない（アリーナがテナント可視の真部分集合になるため）。
#[test]
fn explain_never_reports_full_visible_for_policy_tables() {
    let (core, _g) = hnsw_core("rls13-search-explain", 20, 5);
    let sql = "EXPLAIN SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 5";
    let before = explain_lines(&core, "alice", sql);
    assert!(
        before
            .iter()
            .any(|l| l.starts_with("ann_plan:") && l.contains("full_visible")),
        "ポリシー前は FullVisible: {before:?}"
    );
    enable_keep_policy(&core, "docs");
    let after = explain_lines(&core, "alice", sql);
    assert!(
        after
            .iter()
            .any(|l| l.starts_with("ann_plan:") && l.contains("subset")),
        "適用表では Subset 系: {after:?}"
    );
    let hinted = format!("{sql} HINT ORDER(DISTANCE, SCALAR, RLS)");
    let after_hinted = explain_lines(&core, "alice", &hinted);
    assert!(
        after_hinted.iter().all(|l| !l.contains("full_visible")),
        "DISTANCE 先行でも FullVisible にならない: {after_hinted:?}"
    );
}

// ---------------------------------------------------------------------
// 4. HNSW
// ---------------------------------------------------------------------

/// 近傍の大半をポリシーで隠した大きな表でも、HNSW 経路が `LIMIT k` 件を返し、隠れた行は出ない。
/// ANN 経路（`subset_searches`）を実際に通ることも確かめる。
#[test]
fn hnsw_returns_k_policy_passing_rows_through_the_ann_path() {
    const N: u64 = 2_400;
    const HIDE: u64 = 1_800;
    const K: usize = 10;
    let (core, _g) = hnsw_core("rls13-search-hnsw", N, HIDE);
    enable_keep_policy(&core, "docs");
    let sql = format!("SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT {K}");
    let expected: Vec<u64> = (HIDE + 1..=HIDE + K as u64).collect();
    // 1 回目でベースを用意し、2 回目以降で ANN を通る。
    let mut last = Vec::new();
    for _ in 0..3 {
        last = ranked_ids(&core, "alice", &sql);
        assert_eq!(last.len(), K, "候補が足りる限り LIMIT k 件を返す");
        assert!(
            last.iter().all(|id| *id > HIDE),
            "隠れた行は出ない: {last:?}"
        );
    }
    let overlap = last.iter().filter(|id| expected.contains(id)).count();
    assert!(
        overlap * 10 >= K * 8,
        "厳密解との重なり {overlap}/{K}: {last:?}"
    );
    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.builds >= 1 && stats.subset_searches >= 1,
        "ANN 経路を通っていない（空振り）: {stats:?}"
    );
}

/// HNSW opt-in のハイブリッド密側の再取得ループでも、ポリシーを通る行で `LIMIT k` 件を満たし、
/// 隠れた行は出ない。
#[test]
fn hnsw_hybrid_refetch_loop_fills_limit_with_policy_passing_rows() {
    const N: u64 = 2_400;
    const HIDE: u64 = 1_800;
    const K: usize = 10;
    let (core, _g) = hnsw_core("rls13-search-hnsw-hybrid", N, HIDE);
    enable_keep_policy(&core, "docs");
    let sql =
        format!("SELECT id FROM docs ORDER BY HYBRID(embedding, '[1,0]', body, 'alpha') LIMIT {K}");
    for _ in 0..2 {
        let ids = ranked_ids(&core, "alice", &sql);
        assert_eq!(ids.len(), K);
        assert!(ids.iter().all(|id| *id > HIDE), "隠れた行は出ない: {ids:?}");
    }
    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.hybrid_dense_searches >= 1,
        "ハイブリッド密側が ANN を通っていない: {stats:?}"
    );
}
