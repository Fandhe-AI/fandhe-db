//! `engine::core::EngineCore::execute_sql` の `HINT ORDER(...)` 結合テスト
//! （TASK-76、対象ビヘイビア: SQL-7・RLS-5。ポインタ: `docs/spec/05-tasks.md`
//! TASK-76・`docs/spec/04-behavior/sql-surface.md` SQL-7・
//! `docs/spec/04-behavior/rls.md` RLS-5）。
//!
//! `tests/sql_surface.rs` と同じ流儀（`unique_db_path` / `CleanupGuard`、決定的な
//! 小規模コーパス、厳密な `CpuScalarProvider`）で実 `Storage` 上にテーブルを構築し、
//! 評価順序（既定・`HINT ORDER` 指定）ごとの実行結果を検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
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

/// SQL-2 と同じ under-fetch 検証用コーパス（距離上位に述語不一致行を置く）。
fn setup_lang_corpus(storage: &Storage) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
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
        // TASK-101（RECOVER-10）: 台帳は operation_id ごとに内容ハッシュを持つため、
        // 内容の異なる複数行へ同一 operation_id を使い回すと 2 件目以降が
        // OperationIdContentMismatch で拒否される。行ごとに一意の operation_id を使う。
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &op_id,
        )
        .expect("insert row");
    }
}

fn sql_with_hint(base: &str, hint: Option<&str>) -> String {
    match hint {
        Some(h) => format!("{base} HINT ORDER({h})"),
        None => base.to_string(),
    }
}

// --- 既定順序の正確性（HINT なし == HINT ORDER(RLS, SCALAR, DISTANCE) 明示） ---------

#[test]
fn default_order_and_explicit_rls_scalar_distance_hint_match() {
    let path = unique_db_path("default-matches-explicit");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_lang_corpus(&storage);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let base = "SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2";
    let no_hint = core
        .execute_sql(&ctx, &sql_with_hint(base, None))
        .expect("no-hint execution should succeed");
    let explicit = core
        .execute_sql(&ctx, &sql_with_hint(base, Some("RLS, SCALAR, DISTANCE")))
        .expect("explicit default-order hint execution should succeed");

    assert_eq!(result_ids(&no_hint), vec![3, 4]);
    assert_eq!(result_ids(&no_hint), result_ids(&explicit));
}

// --- SCALAR 先行 4 順列は既定と同一結果（正確に limit 件・under-fetch なし） ---------

#[test]
fn scalar_leading_permutations_match_default_result_exactly() {
    let path = unique_db_path("scalar-leading");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_lang_corpus(&storage);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let base = "SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2";
    for hint in [
        "SCALAR, RLS, DISTANCE",
        "SCALAR, DISTANCE, RLS",
        "RLS, SCALAR, DISTANCE",
    ] {
        let result = core
            .execute_sql(&ctx, &sql_with_hint(base, Some(hint)))
            .unwrap_or_else(|e| panic!("hint={hint:?} execution should succeed: {e}"));
        assert_eq!(result_ids(&result), vec![3, 4], "hint={hint:?}");
    }
}

// --- DISTANCE 先行時の under-fetch 露出（件数 <= limit・返却行は述語を満たす） --------

#[test]
fn distance_leading_permutations_may_under_fetch_but_never_return_mismatched_rows() {
    let path = unique_db_path("distance-leading-underfetch");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_lang_corpus(&storage);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 距離上位 2 件（id=1,2）は共に lang='en'（不一致）のため、DISTANCE 段が先に
    // limit=2 件を確定させると事後 SCALAR フィルタで 0 件まで減り得る
    // （under-fetch の再現。オーバーサンプルによる救済は行わない契約）。
    let base = "SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2";
    for hint in [
        "DISTANCE, SCALAR, RLS",
        "DISTANCE, RLS, SCALAR",
        "RLS, DISTANCE, SCALAR",
    ] {
        let result = core
            .execute_sql(&ctx, &sql_with_hint(base, Some(hint)))
            .unwrap_or_else(|e| panic!("hint={hint:?} execution should succeed: {e}"));
        assert!(
            result.rows.len() <= 2,
            "hint={hint:?} returned more than limit: {:?}",
            result_ids(&result)
        );
        assert_eq!(
            result_ids(&result),
            Vec::<u64>::new(),
            "hint={hint:?}: distance-leading order must expose under-fetch for this corpus"
        );
        // 返却行があれば必ず lang='ja'（不一致行の混入は絶対に許されない）。
        for row in &result.rows {
            let lang_cell = row.cells.last().expect("lang cell present");
            assert_eq!(
                *lang_cell,
                engine::sql::exec::Cell::Text("ja".to_string()),
                "hint={hint:?}: mismatched row must never be returned"
            );
        }
    }
}

// --- RLS 混入 0 件（全 6 順列 × visible() の有無 × 反復） ---------------------------

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
        // TASK-101（RECOVER-10）: 台帳は operation_id ごとに内容ハッシュを持つため、
        // 内容の異なる複数行へ同一 operation_id を使い回すと 2 件目以降が
        // OperationIdContentMismatch で拒否される。行ごとに一意の operation_id を使う。
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            id,
            visibility,
            &[Value::Vector(vec![1.0, 0.0])],
            &op_id,
        )
        .expect("insert row");
    }
}

#[test]
fn no_disallowed_row_leaks_across_all_six_orders_and_visible_predicate_presence() {
    let path = unique_db_path("rls-no-leak-all-orders");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_multi_tenant_table(&storage);
    let core = new_core(storage);

    // tenant-b の Private 行（id=4）だけが不許可（既定は Public のみ許可）。
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let allowed: std::collections::HashSet<u64> = [1u64, 3].into_iter().collect();

    let orders = [
        "RLS, SCALAR, DISTANCE",
        "RLS, DISTANCE, SCALAR",
        "SCALAR, RLS, DISTANCE",
        "SCALAR, DISTANCE, RLS",
        "DISTANCE, RLS, SCALAR",
        "DISTANCE, SCALAR, RLS",
    ];
    for order in orders {
        for where_clause in ["", " WHERE visible()"] {
            let sql = format!(
                "SELECT * FROM docs{where_clause} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10 HINT ORDER({order})"
            );
            for _ in 0..50 {
                let result = core
                    .execute_sql(&ctx, &sql)
                    .unwrap_or_else(|e| panic!("sql={sql:?} execution should succeed: {e}"));
                for id in result_ids(&result) {
                    assert!(
                        allowed.contains(&id),
                        "disallowed row leaked: id={id} sql={sql:?}"
                    );
                    assert_ne!(id, 4, "tenant-b Private row must never leak: sql={sql:?}");
                }
            }
        }
    }
}

// --- ハイブリッド（C4）× HINT ORDER（6 順列すべて成功・不可視行の混入なし） ------------

#[test]
fn hybrid_search_succeeds_and_stays_rls_clean_across_all_six_orders() {
    let path = unique_db_path("hybrid-all-orders");
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

    // clippy::type_complexity 対応（5 要素タプルの配列は複雑すぎるため、
    // このテストローカルな構造体へ分解する）。
    struct HybridRow {
        id: u64,
        tenant: &'static str,
        visibility: Visibility,
        embedding: [f32; 2],
        body: Option<&'static str>,
    }
    let rows = [
        HybridRow {
            id: 1,
            tenant: "tenant-a",
            visibility: Visibility::Public,
            embedding: [1.0, 0.0],
            body: Some("rust vector database"),
        },
        HybridRow {
            id: 2,
            tenant: "tenant-a",
            visibility: Visibility::Public,
            embedding: [0.0, 1.0],
            body: Some("unrelated topic"),
        },
        HybridRow {
            id: 3,
            tenant: "tenant-a",
            visibility: Visibility::Public,
            embedding: [0.9, 0.1],
            body: Some("vector database engine"),
        },
        HybridRow {
            id: 4,
            tenant: "tenant-b",
            visibility: Visibility::Private,
            embedding: [0.95, 0.05],
            body: Some("vector database secret"),
        },
        HybridRow {
            id: 5,
            tenant: "tenant-a",
            visibility: Visibility::Private,
            embedding: [0.1, 0.9],
            body: None,
        },
    ];
    for row in rows {
        let value = match row.body {
            Some(b) => Value::Text(b.to_string()),
            None => Value::Null,
        };
        // テナント境界付き API 経由で投入する（生の `Storage::insert_typed_row` は
        // codex-review P0 指摘・PR #194 対応で `pub(crate)` 化した。`tenant_id` は
        // `PolicyContext` から導出される）。
        let ctx =
            PolicyContext::with_visibilities(row.tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        // TASK-101（RECOVER-10）: 台帳は operation_id ごとに内容ハッシュを持つため、
        // 内容の異なる複数行へ同一 operation_id を使い回すと 2 件目以降が
        // OperationIdContentMismatch で拒否される。行ごとに一意の operation_id を使う。
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{}", row.id))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            row.id,
            row.visibility,
            &[Value::Vector(row.embedding.to_vec()), value],
            &op_id,
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    for order in [
        "RLS, SCALAR, DISTANCE",
        "RLS, DISTANCE, SCALAR",
        "SCALAR, RLS, DISTANCE",
        "SCALAR, DISTANCE, RLS",
        "DISTANCE, RLS, SCALAR",
        "DISTANCE, SCALAR, RLS",
    ] {
        let sql = format!(
            "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 5 HINT ORDER({order})"
        );
        let result = core
            .execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| panic!("order={order:?} hybrid execution should succeed: {e}"));
        for id in result_ids(&result) {
            assert_ne!(
                id, 4,
                "tenant-b Private row must never leak into hybrid results: order={order:?}"
            );
            assert_ne!(
                id, 5,
                "tenant-a Private row (not granted) must never leak: order={order:?}"
            );
        }
    }
}

// --- 拒否側の結合確認（省略・重複・未知段名・位置違反はすべて 42601） -----------------

#[test]
fn execute_sql_rejects_malformed_hint_order_as_syntax_error() {
    let path = unique_db_path("reject-malformed-hint-order");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_lang_corpus(&storage);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let base = "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2";
    for sql in [
        format!("{base} HINT ORDER(RLS, SCALAR)"),
        format!("{base} HINT ORDER(RLS, SCALAR, DISTANCE, RLS)"),
        format!("{base} HINT ORDER(RLS, RLS, SCALAR)"),
        format!("{base} HINT ORDER(RLS, SCALAR, ATTACKER)"),
        format!("{base} HINT ORDER()"),
    ] {
        let err = core
            .execute_sql(&ctx, &sql)
            .expect_err("malformed HINT ORDER must be rejected");
        assert_eq!(err.wire_code(), "42601", "sql={sql:?}");
    }
}

// --- 事前フィルタが HINT ORDER を生き延びることの直接検証（パーサー迂回。
// rls::RlsSafetyNet（TASK-136）の純粋関数テストは crates/engine/src/rls.rs の
// 単体テストで、安全網単体の独立検証（無フィルタ arena による事前フィルタ迂回の
// 模擬）は tests/rls_safety_net.rs でそれぞれカバーしている。ここでは
// `execute_sql` 経由で、`HINT ORDER` が候補構築時の暗黙 RLS 事前フィルタ自体を
// 外せないことを確認する。安全網（`RlsSafetyNet`）は現状この事前フィルタと同じ
// 候補集合から再判定するため、本テストは事前フィルタの効果を検証しており、安全網が
// 独立に不可視行を落としていることの証明ではない） ------------------------------------------------

#[test]
fn distance_leading_hint_order_does_not_bypass_implicit_rls_prefilter() {
    let path = unique_db_path("distance-leading-rls-prefilter");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_multi_tenant_table(&storage);
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // DISTANCE 段を先頭に置いても、候補構築時の暗黙 RLS 事前フィルタにより
    // tenant-b の Private 行（id=4）は絶対に返らない。
    let result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10 HINT ORDER(DISTANCE, SCALAR, RLS)",
        )
        .expect("distance-leading execution should succeed");
    assert!(!result_ids(&result).contains(&4));
    assert_eq!(result_ids(&result), vec![1, 3]);
}
