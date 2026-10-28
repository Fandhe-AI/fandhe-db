//! `engine::core::EngineCore::execute_sql` の結合テスト（TASK-75、対象ビヘイビア:
//! SQL-1, SQL-2, SQL-3, SQL-4。ポインタ: `docs/spec/05-tasks.md` TASK-75・
//! `docs/spec/04-behavior/sql-surface.md`）。
//!
//! `tests/sql_allowlist.rs`・`tests/rls_security.rs` と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、決定的な小規模コーパス、production 判定関数に依存しない独立
//! オラクル）で実 `Storage` 上にテーブルを構築し、`EngineCore::execute_sql` を経由した
//! SQL-1〜4 の受理側実行を検証する。厳密な `CpuScalarProvider`（総当たり）を使うため
//! C1（純粋 Top-k）は Recall@k=1.0 を独立オラクルとの完全一致で確認できる。
//!
//! `EngineCore` は `Storage` を外へ出さない一方向設計のため（`core.rs` モジュール
//! ドキュメント参照）、テストデータの投入は `Storage::open` を直接使い、
//! `EngineCore::from_storage` で束ねてから `execute_sql` を呼ぶ（`EngineCore` 側に
//! テスト専用の storage アクセサを新設しない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{Storage, Visibility};

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

// --- SQL-1: 純粋 Top-k（Recall@k=1.0 を独立オラクルとの完全一致で確認） -----------

#[test]
fn sql1_pure_topk_matches_independent_exact_oracle() {
    let path = unique_db_path("sql1-pure-topk");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let corpus: Vec<(u64, [f32; 3])> = vec![
        (1, [1.0, 0.0, 0.0]),
        (2, [0.9, 0.1, 0.0]),
        (3, [0.0, 1.0, 0.0]),
        (4, [0.0, 0.0, 1.0]),
        (5, [0.5, 0.5, 0.0]),
        (6, [-1.0, 0.0, 0.0]),
    ];
    for (id, emb) in &corpus {
        // テナント境界付き API 経由で投入する（生の `Storage::insert_typed_row` は
        // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した。`tenant_id` は
        // `PolicyContext` から導出される）。
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text("x".to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let query = [1.0f32, 0.0, 0.0];
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
        )
        .expect("SQL-1 execution should succeed");

    // 独立オラクル: f64 で総当たり内積を計算し、スコア降順・同点 id 昇順で Top-3 を選ぶ
    // （production の kernel::CpuScalarProvider とは別経路の再計算。SQL-1 の
    // Recall@20=1.0 相当を、厳密探索との完全一致で確認する）。
    let mut scored: Vec<(u64, f64)> = corpus
        .iter()
        .map(|(id, emb)| {
            let dot: f64 = emb
                .iter()
                .zip(query.iter())
                .map(|(&a, &b)| a as f64 * b as f64)
                .sum();
            (*id, dot)
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let expected: Vec<u64> = scored.into_iter().take(3).map(|(id, _)| id).collect();

    assert_eq!(result_ids(&result), expected);
}

#[test]
fn sql1_rejects_dollar_parameter_placeholder() {
    let path = unique_db_path("sql1-dollar-param");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .execute_sql(&ctx, "SELECT * FROM docs ORDER BY embedding <=> $1 LIMIT 3")
        .expect_err("$n placeholder must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- SQL-2: スカラー条件付き Top-k（under-fetch なしの事前フィルタ） ------------------

#[test]
fn sql2_where_equality_excludes_non_matching_rows_without_under_fetch() {
    let path = unique_db_path("sql2-where-equality");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    // 距離上位に lang != 'ja' の行を意図的に置く。distance-first の under-fetch が
    // あれば、LIMIT 2 の枠を不一致行が占有して 'ja' 行が漏れる。
    let rows: [(u64, [f32; 2], &str); 5] = [
        (1, [1.0, 0.0], "en"),  // 最近傍だが不一致
        (2, [0.99, 0.0], "en"), // 2 番目に近いが不一致
        (3, [0.9, 0.0], "ja"),
        (4, [0.8, 0.0], "ja"),
        (5, [0.0, 1.0], "ja"),
    ];
    for (id, emb, lang) in rows {
        // テナント境界付き API 経由で投入する（生の `Storage::insert_typed_row` は
        // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した。`tenant_id` は
        // `PolicyContext` から導出される）。
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2",
        )
        .expect("SQL-2 execution should succeed");

    assert_eq!(result_ids(&result), vec![3, 4]);
}

// --- SQL-3: RLS 適用 Top-k（`visible()` の有無に依存しない無条件強制） ---------------

fn setup_multi_tenant_table(storage: &Storage) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let rows: [(u64, &str, Visibility); 4] = [
        (1, "tenant-a", Visibility::Public),
        (2, "tenant-a", Visibility::Private),
        (3, "tenant-b", Visibility::Public),
        (4, "tenant-b", Visibility::Private),
    ];
    for (id, tenant, visibility) in rows {
        // テナント境界付き API 経由で投入する（生の `Storage::insert_typed_row` は
        // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した。`tenant_id` は
        // `PolicyContext` から導出される）。
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            id,
            visibility,
            &[Value::Vector(vec![1.0, 0.0])],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
}

#[test]
fn sql3_rls_is_enforced_regardless_of_visible_predicate_presence() {
    let path = unique_db_path("sql3-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_multi_tenant_table(&storage);
    let core = new_core(storage);

    // tenant-a・Public のみ許可（Private は明示付与なし）。
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    for sql in [
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        "SELECT * FROM docs WHERE visible() ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
    ] {
        let result = core
            .execute_sql(&ctx, sql)
            .unwrap_or_else(|e| panic!("SQL-3 execution should succeed for {sql:?}: {e}"));
        // tenant-a 自身の行（Public）と、他テナントの Public 行が可視（既定は Public
        // のみ許可。`policy.rs::PolicyContext::is_visible` の契約どおり、Public は
        // テナントをまたいで可視）。Private 行（id=2, id=4）の混入は 0 件。
        assert_eq!(result_ids(&result), vec![1, 3], "sql={sql:?}");
    }
}

#[test]
fn sql3_no_disallowed_row_leaks_across_repeated_trials() {
    let path = unique_db_path("sql3-leak-trials");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_multi_tenant_table(&storage);
    let core = new_core(storage);

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    // Public を明示付与しているため他テナントの Public 行（id=3）も可視。
    // 不許可（漏れてはならない）のは tenant-b の Private 行（id=4）のみ。
    let allowed: std::collections::HashSet<u64> = [1u64, 2, 3].into_iter().collect();

    for _ in 0..50 {
        let result = core
            .execute_sql(
                &ctx,
                "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
            )
            .expect("SQL-3 execution should succeed");
        for id in result_ids(&result) {
            assert!(allowed.contains(&id), "disallowed row leaked: id={id}");
        }
    }
}

// --- SQL-4: ハイブリッド（関数形・専用構文形の完全一致） ---------------------------

#[test]
fn sql4_hybrid_rrf_and_hybrid_syntax_forms_return_identical_topk() {
    let path = unique_db_path("sql4-hybrid-equivalence");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    let rows: [(u64, [f32; 2], Option<&str>); 4] = [
        (1, [1.0, 0.0], Some("rust vector database")),
        (2, [0.0, 1.0], Some("unrelated topic")),
        (3, [0.9, 0.1], Some("vector database engine")),
        (4, [0.1, 0.9], None),
    ];
    for (id, emb, body) in rows {
        let value = match body {
            Some(b) => Value::Text(b.to_string()),
            None => Value::Null,
        };
        // テナント境界付き API 経由で投入する（生の `Storage::insert_typed_row` は
        // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した。`tenant_id` は
        // `PolicyContext` から導出される）。
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), value],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql_fn = "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 4";
    let sql_kw = "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 4";

    let result_fn = core.execute_sql(&ctx, sql_fn).expect("hybrid_rrf form");
    let result_kw = core.execute_sql(&ctx, sql_kw).expect("HYBRID form");

    assert_eq!(result_ids(&result_fn), result_ids(&result_kw));
    // 密近傍（id=1）・キーワード一致（id=1,3）がいずれも上位に含まれる。
    assert!(result_ids(&result_fn).contains(&1));
}

#[test]
fn sql4_two_arg_form_is_rejected_as_not_executable() {
    let path = unique_db_path("sql4-two-arg");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let err = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, 'query text') LIMIT 5",
        )
        .expect_err("2-arg hybrid form must be rejected at bind time");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn sql4_hybrid_degrades_to_dense_only_when_no_visible_body_text() {
    let path = unique_db_path("sql4-dense-degrade");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    for (id, emb) in [(1u64, [1.0f32, 0.0]), (2, [0.0, 1.0])] {
        // テナント境界付き API 経由で投入する（生の `Storage::insert_typed_row` は
        // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した。`tenant_id` は
        // `PolicyContext` から導出される）。
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Null],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'anything') LIMIT 2",
        )
        .expect("hybrid must degrade to dense-only, not fail, when no body text is visible");
    assert_eq!(result_ids(&result), vec![1, 2]);
}

// TASK-84（対応 Issue #61）: SQL-4（HYBRID/hybrid_rrf 経由の RRF 融合）end-to-end で、
// 同点融合スコアを持つ行群が `LIMIT` 境界を跨ぐ場合でも決定的であることを検証する。
// `hybrid.rs`・`crates/engine/tests/hybrid.rs` のユニット・統合レベルの検証を
// SQL 表層（`EngineCore::execute_sql`）まで通しで確認する回帰テスト。
//
// SQL 表層の密側候補プールは `pool_depth = LIMIT.max(200)`（`sql/exec.rs` 参照）で
// 可視行数を常に上回るため、可視行はすべて密側ランキングに含まれる（密側から
// 完全に除外される行は作れない。`crates/engine/tests/hybrid.rs` のユニットテストが
// 使う「密側/疎側どちらかのみで得点させる」構成は SQL 表層では使えない）。そこで
// 本テストは「密ランクと疎ランクを入れ替えた 2 行」で融合後スコアの完全一致
// （タイ）を作る: `RrfConfig` は密・疎の重み・`k_const` が共通のため、行 A
// （密 rank i・疎 rank j）と行 B（密 rank j・疎 rank i）は
// `1/(k_const+i+1) + 1/(k_const+j+1)` の加算が可換であることから、i と j の
// 具体値によらず浮動小数点として完全一致する。
#[test]
fn sql4_hybrid_tie_group_across_limit_boundary_is_deterministic() {
    let path = unique_db_path("sql4-tie-boundary");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    // id=1（行 A）: 密ランク 0（内積最大）・疎ランク 1（tf=1、文書長 3 語）。
    // id=2（行 B）: 密ランク 1（内積 2 位）・疎ランク 0（tf=3、文書長 3 語で
    // id=1 と揃えているため BM25 は tf のみで単調に順位付けされる）。
    // 密・疎のランクの組が入れ替わっているため融合スコアは常に完全一致する
    // （上記コメント参照）。id=90〜92 は `anchor` を含まない埋め草行で、密内積を
    // 行 A・B より小さくして密ランク 2 以降に押し出す（タイ組の構成に影響しない）。
    let rows: [(u64, [f32; 2], Option<&str>); 5] = [
        (1, [1.0, 0.0], Some("anchor filler filler")),
        (2, [0.9, 0.0], Some("anchor anchor anchor")),
        (90, [0.1, 0.0], None),
        (91, [0.05, 0.0], None),
        (92, [0.01, 0.0], None),
    ];
    for (id, emb, body) in rows {
        let value = match body {
            Some(b) => Value::Text(b.to_string()),
            None => Value::Null,
        };
        // テナント境界付き API 経由で投入する（生の `Storage::insert_typed_row` は
        // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した。`tenant_id` は
        // `PolicyContext` から導出される）。
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), value],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    // LIMIT 1 は行 A・B の完全同点タイ組（id 1, 2）のうち id 昇順で先頭の id=1 のみを
    // 含む位置になる。すなわちタイ組の途中を LIMIT が横切る。
    let sql =
        "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'anchor') LIMIT 1";

    let baseline = core.execute_sql(&ctx, sql).expect("hybrid SQL-4 ok");
    let baseline_ids = result_ids(&baseline);
    assert_eq!(
        baseline_ids,
        vec![1u64],
        "tie group {{1,2}} must be cut after the smaller id"
    );
    for trial in 1..20 {
        let result = core
            .execute_sql(&ctx, sql)
            .unwrap_or_else(|e| panic!("trial={trial}: hybrid SQL-4 ok, got {e}"));
        assert_eq!(
            result_ids(&result),
            baseline_ids,
            "trial={trial} diverged from baseline"
        );
    }

    // LIMIT 2 ならタイ組（id 1, 2）を全件含み、id 昇順で並ぶことも確認する。
    let sql_full_group =
        "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'anchor') LIMIT 2";
    let full_group = core
        .execute_sql(&ctx, sql_full_group)
        .expect("hybrid SQL-4 ok (full tie group)");
    assert_eq!(result_ids(&full_group), vec![1u64, 2u64]);

    // 2 構文形（`hybrid_rrf(...)` / `HYBRID(...)`）でも同一 Top-k であることを
    // 同時に確認する（既存の
    // `sql4_hybrid_rrf_and_hybrid_syntax_forms_return_identical_topk` と同種の
    // 検証を、同点境界コーパスでも独立に確認する）。
    let sql_kw =
        "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'anchor') LIMIT 1";
    let result_kw = core.execute_sql(&ctx, sql_kw).expect("HYBRID form ok");
    assert_eq!(result_ids(&result_kw), baseline_ids);
}

// --- 共通契約: 空テーブル・SELECT * の列順 -------------------------------------

#[test]
fn empty_table_returns_empty_result() {
    let path = unique_db_path("empty-table");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5",
        )
        .expect("empty table should succeed with an empty result");
    assert!(result.rows.is_empty());
}

#[test]
fn select_star_projects_id_then_schema_column_order() {
    let path = unique_db_path("select-star-order");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    // テナント境界付き API 経由で投入する（生の `Storage::insert_typed_row` は
    // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した。`tenant_id` は
    // `PolicyContext` から導出される）。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        7,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("hello".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert row");
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
        )
        .expect("select * should succeed");
    let row = result.rows.first().expect("one row");
    assert_eq!(row.id, 7);
    assert_eq!(
        row.cells,
        vec![
            Cell::Integer(7),
            Cell::Vector(vec![1.0, 0.0]),
            Cell::Text("hello".to_string()),
        ]
    );
}

// --- Issue #56 レビュー指摘対応・codex P1: `decode_scalar_columns` の全列無条件
// 確保を廃し、投影・WHERE フィルタ・hybrid 本文として必要な列だけを選択的に
// 複製する設計への切り替え。本結合テストは `row_codec::scan_scalar_columns` の
// 単体テスト（ポインタが `buf` 借用であること）を補完し、投影に無関係な巨大
// `Text` 列を持つ可視行に対して、実際の SQL 実行結果（`SELECT id ...` が値を返す・
// WHERE フィルタが投影に含まれない列でも正しく効く）が正しいことを確認する。

#[test]
fn sql2_select_id_ignores_unprojected_large_text_column() {
    let path = unique_db_path("sql2-select-id-large-column");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                // 投影にも WHERE 条件にも現れない大容量 Text 列。旧実装ではこの列も
                // `on_visible_row` の走査ごとに無条件で `to_string()` 確保していた。
                ColumnDef::new("bio", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    // 距離上位に lang != 'ja' の行を意図的に置く（sql2_where_equality と同じ
    // under-fetch 検証意図に、投影に不要な大容量列を追加した構成）。
    let large_bio = "x".repeat(1_000_000);
    let rows: [(u64, [f32; 2], &str); 5] = [
        (1, [1.0, 0.0], "en"),
        (2, [0.99, 0.0], "en"),
        (3, [0.9, 0.0], "ja"),
        (4, [0.8, 0.0], "ja"),
        (5, [0.0, 1.0], "ja"),
    ];
    for (id, emb, lang) in rows {
        // テナント境界付き API 経由で投入する（生の `Storage::insert_typed_row` は
        // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した。`tenant_id` は
        // `PolicyContext` から導出される）。
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(large_bio.clone()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2",
        )
        .expect("SELECT id with unrelated large Text column should succeed");

    assert_eq!(result_ids(&result), vec![3, 4]);
    for row in &result.rows {
        assert_eq!(row.cells, vec![Cell::Integer(row.id)]);
    }
}
