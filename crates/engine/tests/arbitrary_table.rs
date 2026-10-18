//! 任意テーブルでの SQL 実行検証（TASK-81、対象ビヘイビア: SQL-11。ポインタ:
//! `docs/spec/05-tasks.md` TASK-81・`docs/spec/04-behavior/sql-surface.md` SQL-11）。
//!
//! `tests/sql_surface.rs`（TASK-75・SQL-1〜4）・`tests/sql_evaluation_order.rs`
//! （TASK-76・SQL-7）・`tests/rls_generalized.rs`（TASK-138・RLS-8）・
//! `tests/recovery_ledger.rs`（TASK-93・RECOVER-2）・`tests/sql_operation_id.rs`
//! （TASK-80・SQL-10）・`tests/multi_dim_tables.rs`（TASK-91・TABLE-2）は、いずれも
//! `documents` 相当の単一テーブル、または production の可視性判定 API から独立した
//! 一部の契約のみを個別に検証している。本ファイルは、`CREATE TABLE` で作成した
//! `documents` 以外の任意テーブル（複数・複数次元）に対して、上記ファイル群が
//! 検証済みの契約（C1〜C4 の単一文表現・評価順序・RLS 暗黙適用・テーブル単位
//! `operation_id` 台帳・`INSERT`）が同一に成立することを 1 ファイルへ集約して
//! 機械検証し、SQL-11 の確定化判定の根拠を作る（`sql/exec.rs`・`parser.rs` は
//! FROM のテーブル名を `catalog.rs` 経由で解決しており、実行経路に `documents` の
//! ハードコードは存在しないことを踏まえた対照検証）。
//!
//! 上記各ファイルと同じ流儀（`unique_db_path` / `CleanupGuard`、実 `Storage` +
//! `CpuScalarProvider` + `EngineCore::from_storage`、決定的な小規模コーパス、
//! production 判定関数に依存しない独立オラクル）に従う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

fn op(id: &str) -> OperationId {
    OperationId::parse(id).expect("valid operation_id")
}

/// `embedding: Vector(dim)` / `body: Text` / `lang: Text(nullable)` を持つ
/// `TableSchema` を任意テーブル名で生成する（`sql_surface.rs`・
/// `sql_evaluation_order.rs` が使う列構成を任意テーブル向けに一般化したもの）。
fn schema(table: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        table,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

/// `sql_surface.rs`・`sql_evaluation_order.rs` と同一の内容（コーパス・可視性）を
/// 任意テーブル名へ投入する（`documents` 側との構造一致を確認するための共有経路）。
struct SeedRow {
    id: u64,
    embedding: [f32; 2],
    lang: &'static str,
}

const SEED_ROWS: [SeedRow; 5] = [
    SeedRow {
        id: 1,
        embedding: [1.0, 0.0],
        lang: "en",
    },
    SeedRow {
        id: 2,
        embedding: [0.99, 0.0],
        lang: "en",
    },
    SeedRow {
        id: 3,
        embedding: [0.9, 0.0],
        lang: "ja",
    },
    SeedRow {
        id: 4,
        embedding: [0.8, 0.0],
        lang: "ja",
    },
    SeedRow {
        id: 5,
        embedding: [0.0, 1.0],
        lang: "ja",
    },
];

fn seed_corpus(storage: &Storage, table: &str, tenant: &str) {
    for row in &SEED_ROWS {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        // TASK-94・RECOVER-3 の重複拒否（`23505`）はテーブル単位で `operation_id` を
        // 一意に要求するため、同一テーブル・同一テナントへの複数行 seed は行ごとに
        // 別の `operation_id` を使う（PR #247 codex-review 指摘対応。1 つの
        // `operation_id` を複数行の insert で使い回す既存の記述は TASK-93（keep-first
        // のみ）時点のものでもはや成立しない）。
        engine::tenant::insert_typed_row(
            storage,
            table,
            &ctx,
            row.id,
            Visibility::Public,
            &[
                Value::Vector(row.embedding.to_vec()),
                Value::Text(format!("body-{}", row.id)),
                Value::Text(row.lang.to_string()),
            ],
            // 台帳の内容照合（TASK-101・RECOVER-10）は operation_id 単位でハッシュを
            // 持つため、コーパス内の各行（内容が異なる）へ同一 operation_id を使い回すと
            // 2 行目以降が `OperationIdContentMismatch` になる。行ごと・テーブルごとに
            // 一意な operation_id を割り当てる。
            &op(&format!("seed-op-{table}-{}", row.id)),
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

// --- 1. C1〜C4 の同一契約（`documents` との構造一致） -----------------------------

#[test]
fn arbitrary_table_c1_pure_topk_matches_documents_and_independent_oracle() {
    let path = unique_db_path("arb-c1-topk");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("documents", 2))
        .expect("create documents");
    storage
        .create_table(&schema("kb_articles", 2))
        .expect("create kb_articles");
    seed_corpus(&storage, "documents", "tenant-a");
    seed_corpus(&storage, "kb_articles", "tenant-a");

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql =
        |table: &str| format!("SELECT * FROM {table} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3");

    let documents = core
        .execute_sql(&ctx, &sql("documents"))
        .expect("documents C1 should succeed");
    let kb_articles = core
        .execute_sql(&ctx, &sql("kb_articles"))
        .expect("kb_articles C1 should succeed");

    // 独立オラクル: f64 で総当たり内積を計算し、スコア降順・同点 id 昇順で Top-3 を選ぶ
    // （`sql_surface.rs::sql1_pure_topk_matches_independent_exact_oracle` と同じ方式）。
    let query = [1.0f64, 0.0];
    let mut scored: Vec<(u64, f64)> = SEED_ROWS
        .iter()
        .map(|row| {
            let dot: f64 = row
                .embedding
                .iter()
                .zip(query.iter())
                .map(|(&a, &b)| a as f64 * b)
                .sum();
            (row.id, dot)
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let expected: Vec<u64> = scored.into_iter().take(3).map(|(id, _)| id).collect();

    assert_eq!(result_ids(&documents), expected);
    assert_eq!(
        result_ids(&kb_articles),
        expected,
        "arbitrary table must match documents' structural contract"
    );
}

#[test]
fn arbitrary_table_c2_where_equality_matches_documents() {
    let path = unique_db_path("arb-c2-where");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("documents", 2))
        .expect("create documents");
    storage
        .create_table(&schema("kb_articles", 2))
        .expect("create kb_articles");
    seed_corpus(&storage, "documents", "tenant-a");
    seed_corpus(&storage, "kb_articles", "tenant-a");

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql = |table: &str| {
        format!(
            "SELECT * FROM {table} WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2"
        )
    };

    let documents = core
        .execute_sql(&ctx, &sql("documents"))
        .expect("documents C2 should succeed");
    let kb_articles = core
        .execute_sql(&ctx, &sql("kb_articles"))
        .expect("kb_articles C2 should succeed");

    assert_eq!(result_ids(&documents), vec![3, 4]);
    assert_eq!(result_ids(&kb_articles), result_ids(&documents));
}

#[test]
fn arbitrary_table_c3_visible_predicate_does_not_change_result() {
    let path = unique_db_path("arb-c3-visible");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("kb_articles", 2))
        .expect("create table");
    seed_corpus(&storage, "kb_articles", "tenant-a");

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let no_visible = core
        .execute_sql(
            &ctx,
            "SELECT * FROM kb_articles ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5",
        )
        .expect("without visible() should succeed");
    let with_visible = core
        .execute_sql(
            &ctx,
            "SELECT * FROM kb_articles WHERE visible() ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5",
        )
        .expect("with visible() should succeed");
    // 独立オラクル: SEED_ROWS の embedding は問い合わせベクトル [1.0,0.0] からの
    // 距離が id の昇順（1 が最近傍・5 が最遠）になるよう構成済みのため、期待順は
    // 固定順 [1,2,3,4,5] に確定する（空集合同士の一致で通過する検証にしない）。
    let expected = vec![1u64, 2, 3, 4, 5];
    assert_eq!(result_ids(&no_visible), expected);
    assert_eq!(result_ids(&with_visible), expected);
}

#[test]
fn arbitrary_table_c4_hybrid_rrf_and_hybrid_syntax_forms_match_documents() {
    let path = unique_db_path("arb-c4-hybrid");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("documents", 2))
        .expect("create documents");
    storage
        .create_table(&schema("kb_articles", 2))
        .expect("create kb_articles");

    let rows: [(u64, [f32; 2], &str); 3] = [
        (1, [1.0, 0.0], "rust vector database"),
        (2, [0.0, 1.0], "unrelated topic"),
        (3, [0.9, 0.1], "vector database engine"),
    ];
    for table in ["documents", "kb_articles"] {
        for (id, emb, body) in rows {
            let ctx = PolicyContext::with_visibilities(
                "tenant-a",
                [Visibility::Public, Visibility::Private],
            )
            .expect("valid tenant");
            engine::tenant::insert_typed_row(
                &storage,
                table,
                &ctx,
                id,
                Visibility::Public,
                &[
                    Value::Vector(emb.to_vec()),
                    Value::Text(body.to_string()),
                    Value::Text("en".to_string()),
                ],
                // 台帳の内容照合（TASK-101・RECOVER-10）は operation_id 単位でハッシュを
                // 持つため、行ごとに内容が異なる本ループでは一意な operation_id を使う
                // （table・id ごとに衝突しない値）。
                &op(&format!("hybrid-seed-{table}-{id}")),
            )
            .expect("insert row");
        }
    }

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql_fn = |table: &str| {
        format!(
            "SELECT * FROM {table} ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 3"
        )
    };
    let sql_kw = |table: &str| {
        format!(
            "SELECT * FROM {table} ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 3"
        )
    };

    let documents_fn = core
        .execute_sql(&ctx, &sql_fn("documents"))
        .expect("documents hybrid_rrf form should succeed");
    let documents_kw = core
        .execute_sql(&ctx, &sql_kw("documents"))
        .expect("documents HYBRID form should succeed");
    let kb_fn = core
        .execute_sql(&ctx, &sql_fn("kb_articles"))
        .expect("kb_articles hybrid_rrf form should succeed");
    let kb_kw = core
        .execute_sql(&ctx, &sql_kw("kb_articles"))
        .expect("kb_articles HYBRID form should succeed");

    assert_eq!(result_ids(&documents_fn), result_ids(&documents_kw));
    assert_eq!(result_ids(&kb_fn), result_ids(&kb_kw));
    assert_eq!(
        result_ids(&kb_fn),
        result_ids(&documents_fn),
        "arbitrary table hybrid result must match documents"
    );
    // 独立オラクル: id=1 は問い合わせベクトル [1.0,0.0] に対する厳密最近傍（距離 0）
    // であり、かつ問い合わせキーワード「vector database」を本文にそのまま含む
    // ため、ベクトル順位・テキスト順位のいずれでも最上位（rank=1）を占め、RRF 融合
    // 後も先頭に来ることが確定する。相互比較のみで空集合同士も一致してしまう
    // vacuous pass を防ぐため、絶対件数・先頭要素を明示検証する。
    assert!(
        !result_ids(&documents_fn).is_empty(),
        "hybrid_rrf must return at least one row for this corpus"
    );
    assert_eq!(
        result_ids(&documents_fn).first(),
        Some(&1u64),
        "id=1 is the closest vector match and contains the exact keyword phrase, \
         so it must rank first after RRF fusion"
    );
}

// --- 2. 評価順序（SQL-7 契約の任意テーブル適用） -----------------------------------

#[test]
fn arbitrary_table_evaluation_order_default_and_scalar_leading_match() {
    let path = unique_db_path("arb-eval-order-default");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("kb_articles", 2))
        .expect("create table");
    seed_corpus(&storage, "kb_articles", "tenant-a");

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let base =
        "SELECT * FROM kb_articles WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2";

    let no_hint = core
        .execute_sql(&ctx, &sql_with_hint(base, None))
        .expect("no-hint execution should succeed");
    assert_eq!(result_ids(&no_hint), vec![3, 4]);

    for hint in [
        "RLS, SCALAR, DISTANCE",
        "SCALAR, RLS, DISTANCE",
        "SCALAR, DISTANCE, RLS",
    ] {
        let result = core
            .execute_sql(&ctx, &sql_with_hint(base, Some(hint)))
            .unwrap_or_else(|e| panic!("hint={hint:?} execution should succeed: {e}"));
        assert_eq!(result_ids(&result), vec![3, 4], "hint={hint:?}");
    }
}

#[test]
fn arbitrary_table_distance_leading_may_under_fetch_but_never_returns_mismatched_rows() {
    let path = unique_db_path("arb-eval-order-underfetch");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("kb_articles", 2))
        .expect("create table");
    seed_corpus(&storage, "kb_articles", "tenant-a");

    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let base =
        "SELECT * FROM kb_articles WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2";

    for hint in [
        "DISTANCE, SCALAR, RLS",
        "DISTANCE, RLS, SCALAR",
        "RLS, DISTANCE, SCALAR",
    ] {
        let result = core
            .execute_sql(&ctx, &sql_with_hint(base, Some(hint)))
            .unwrap_or_else(|e| panic!("hint={hint:?} execution should succeed: {e}"));
        assert!(result.rows.len() <= 2, "hint={hint:?} exceeded limit");
        // 独立オラクル: 問い合わせベクトル [1.0,0.0] に対する距離上位 2 件（id=1,2）は
        // 共に lang='en'（WHERE lang='ja' に不一致）であるようコーパスを構成済み。
        // DISTANCE 段が先に limit=2 件を確定させると事後スカラーフィルタで 0 件まで
        // 減ることが確定するため、0 行を明示検証する（`sql_evaluation_order.rs` の
        // 同型テストと同じ方式）。これにより空集合が無条件で通過する vacuous pass を
        // 防ぐ。
        assert_eq!(
            result_ids(&result),
            Vec::<u64>::new(),
            "hint={hint:?}: distance-leading order must expose under-fetch for this corpus"
        );
        for row in &result.rows {
            let lang_cell = row.cells.last().expect("lang cell present");
            assert_eq!(
                *lang_cell,
                Cell::Text("ja".to_string()),
                "hint={hint:?}: mismatched row must never be returned"
            );
        }
    }
}

// --- 3. RLS 暗黙適用（混入 0 件。全 6 順列 × 複数テナント・複数 Visibility） --------

struct RlsRow {
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
}

const RLS_ROWS: [RlsRow; 4] = [
    RlsRow {
        id: 1,
        tenant: "tenant-a",
        visibility: Visibility::Public,
    },
    RlsRow {
        id: 2,
        tenant: "tenant-a",
        visibility: Visibility::Private,
    },
    RlsRow {
        id: 3,
        tenant: "tenant-b",
        visibility: Visibility::Public,
    },
    RlsRow {
        id: 4,
        tenant: "tenant-b",
        visibility: Visibility::Private,
    },
];

fn setup_multi_tenant_table(storage: &Storage, table: &str) {
    storage
        .create_table(&TableSchema::new(
            table,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    for row in &RLS_ROWS {
        let ctx =
            PolicyContext::with_visibilities(row.tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            storage,
            table,
            &ctx,
            row.id,
            row.visibility,
            &[Value::Vector(vec![1.0, 0.0])],
            // 台帳の内容照合（TASK-101・RECOVER-10）は (tenant, table, operation_id)
            // 単位でハッシュを持つため、テナントをまたいで内容が異なる本ループでは
            // 一意な operation_id を使う。
            &op(&format!("rls-seed-{table}-{}", row.id)),
        )
        .expect("insert row");
    }
}

#[test]
fn arbitrary_table_rls_no_disallowed_row_leaks_across_all_six_orders() {
    let path = unique_db_path("arb-rls-no-leak");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    setup_multi_tenant_table(&storage, "kb_articles");
    let core = new_core(storage);

    // tenant-a 視点: 自身の Public/Private（id=1,2）と tenant-b の Public（id=3）が
    // 許可。不許可（漏れてはならない）のは tenant-b の Private（id=4）のみ。
    // 独立オラクル: production の `PolicyContext::is_visible` を経由せず、期待値を
    // テスト側でベタ書きする（RLS-8 の一般化検証と同じ方式）。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let allowed: std::collections::HashSet<u64> = [1u64, 2, 3].into_iter().collect();

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
                "SELECT * FROM kb_articles{where_clause} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10 HINT ORDER({order})"
            );
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
            // 下限アサーション: LIMIT 10 に対しコーパスは 4 行のみ・スカラー述語も
            // 距離順によるオーバーサンプルの余地もないため、許可される 3 件
            // （id=1,2,3）は必ず全件返る。空集合が無条件で通過する vacuous pass を
            // 防ぐため、否定的チェックだけでなく実際の返却集合を厳密一致で検証する。
            let actual: std::collections::HashSet<u64> = result_ids(&result).into_iter().collect();
            assert_eq!(
                actual, allowed,
                "expected exactly the allowed set to be returned: sql={sql:?}"
            );
        }
    }
}

// --- 4. `INSERT ... USING OPERATION_ID`（SQL-10 契約の任意テーブル適用） -----------

#[test]
fn arbitrary_table_insert_with_operation_id_is_accepted_and_readable() {
    let path = unique_db_path("arb-insert-accept");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("kb_articles", 3))
        .expect("create table");
    let core = new_core(storage);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let outcome = core
        .execute_insert_sql(
            &write_ctx,
            "INSERT INTO kb_articles (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .expect("insert with clause should succeed on arbitrary table");
    assert_eq!(outcome.rows_affected, 1);

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id, body FROM kb_articles ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(result_ids(&result), vec![1]);
}

#[test]
fn arbitrary_table_insert_missing_operation_id_clause_is_rejected_before_any_write() {
    let path = unique_db_path("arb-insert-missing-clause");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("kb_articles", 3))
        .expect("create table");
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO kb_articles (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello')",
        )
        .expect_err("missing clause must be rejected on arbitrary table");
    assert_eq!(err.wire_code(), "23502");

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM kb_articles ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert!(
        result.rows.is_empty(),
        "no row must be written when the clause is missing"
    );
}

// --- 5. テーブル単位台帳（RECOVER-2 契約の任意テーブル適用） ----------------------

#[test]
fn arbitrary_table_ledger_scope_is_per_table_and_per_tenant() {
    let path = unique_db_path("arb-ledger-scope");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("kb_articles", 3))
        .expect("create kb_articles");
    storage
        .create_table(&schema("faq_entries", 3))
        .expect("create faq_entries");
    let core = new_core(storage);
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

    core.execute_insert_sql(
        &ctx_a,
        "INSERT INTO kb_articles (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-shared'",
    )
    .expect("insert into kb_articles must succeed");

    // 同一 operation_id を異なる任意テーブルへ使うと双方成功する（スコープ独立）。
    core.execute_insert_sql(
        &ctx_a,
        "INSERT INTO faq_entries (id, embedding, body) VALUES (1, '[0.4,0.5,0.6]', 'b') USING OPERATION_ID 'op-shared'",
    )
    .expect("reusing the same operation_id on a different arbitrary table must succeed");

    assert_eq!(
        core.operation_recorded(&ctx_a, "kb_articles", &op("op-shared"))
            .expect("lookup ok"),
        LedgerLookup::Recorded
    );
    assert_eq!(
        core.operation_recorded(&ctx_a, "faq_entries", &op("op-shared"))
            .expect("lookup ok"),
        LedgerLookup::Recorded
    );

    // 既存行 id=1 への再送は 23505（行キー衝突）。これは operation_id とは無関係な
    // 行ストア側の重複検出であることを明示するため、あえて未使用の operation_id
    // （op-resend）を使う（重複行 id 検出は operation_id の有無に分岐しない。
    // `tenant.rs::insert_row` ドキュメント参照）。
    let err = core
        .execute_insert_sql(
            &ctx_a,
            "INSERT INTO kb_articles (id, embedding, body) VALUES (1, '[0.9,0.9,0.9]', 'overwrite') USING OPERATION_ID 'op-resend'",
        )
        .expect_err("row id conflict must be rejected regardless of operation_id");
    assert_eq!(err.wire_code(), "23505");

    // 同一テーブル・同一テナントへの operation_id 自体の再送（op-shared を新規行
    // id=2・異なる内容へ再利用）。TASK-93（本台帳）は「既存エントリを上書きしない
    // （keep-first）」ことのみを契約とし、重複行 id 拒否（23505）は TASK-94・
    // RECOVER-3 の管轄だが、内容照合（TASK-101・RECOVER-10）は operation_id 単位で
    // 記録済みハッシュと比較するため、id・内容のいずれかが異なれば
    // `OperationIdContentMismatch`（`22023`）として拒否される（`recovery/ledger.rs`
    // モジュール冒頭ドキュメント参照。行 id 衝突とは独立した判定軸であることの確認。
    // 同一内容の再送が `23505` になることは `tests/recovery_content_hash.rs` が
    // 別途検証する）。
    let err_content_mismatch = core
        .execute_insert_sql(
            &ctx_a,
            "INSERT INTO kb_articles (id, embedding, body) VALUES (2, '[0.3,0.3,0.3]', 'resend') USING OPERATION_ID 'op-shared'",
        )
        .expect_err(
            "resending an already-recorded operation_id with different content must be rejected",
        );
    assert_eq!(err_content_mismatch.wire_code(), "22023");
    assert_eq!(
        core.operation_recorded(&ctx_a, "kb_articles", &op("op-shared"))
            .expect("lookup ok"),
        LedgerLookup::Recorded,
        "keep-first: the ledger entry recorded by the first op-shared write must remain"
    );
    assert!(
        matches!(
            core.get_row(&ctx_a, "kb_articles", "tenant-a", 2),
            Err(CoreError::NotFound)
        ),
        "the rejected resend must not have written row id=2"
    );

    // tenant-b が同一テーブルへ同一 operation_id を使っても成功する（テナント単位
    // スコープが独立している）。行キー (tenant_id, id) もテナント単位で名前空間化
    // されているため、insert の成否だけでは台帳のテナントスコープを検証できない
    // （Bugbot 指摘対応）。tenant-b 視点で op-shared が未使用であることを事前に、
    // 使用済みであることを事後に `operation_recorded` で直接確認する。
    assert_eq!(
        core.operation_recorded(&ctx_b, "kb_articles", &op("op-shared"))
            .expect("lookup ok"),
        LedgerLookup::NotRecorded,
        "tenant-b must not observe tenant-a's op-shared ledger entry before its own write"
    );
    core.execute_insert_sql(
        &ctx_b,
        "INSERT INTO kb_articles (id, embedding, body) VALUES (1, '[0.7,0.7,0.7]', 'b-owned') USING OPERATION_ID 'op-shared'",
    )
    .expect("tenant-b reusing the same operation_id must succeed (different tenant namespace)");
    assert_eq!(
        core.operation_recorded(&ctx_b, "kb_articles", &op("op-shared"))
            .expect("lookup ok"),
        LedgerLookup::Recorded,
        "tenant-b's own op-shared write must now be observable in tenant-b's own namespace"
    );

    // 他テナントの同一 operation_id の存在は、自テナントの 23505 判定に影響しない
    // （台帳照合が存在オラクルにならないことの確認）。
    let err_after = core
        .execute_insert_sql(
            &ctx_a,
            "INSERT INTO kb_articles (id, embedding, body) VALUES (1, '[0.2,0.2,0.2]', 'again') USING OPERATION_ID 'op-another'",
        )
        .expect_err("row id conflict must persist regardless of another tenant's ledger entries");
    assert_eq!(err_after.wire_code(), "23505");

    // `EngineCore::operation_recorded` はテーブル単位で Recorded/NotRecorded を返す。
    assert_eq!(
        core.operation_recorded(&ctx_a, "kb_articles", &op("op-unused"))
            .expect("lookup ok"),
        LedgerLookup::NotRecorded
    );
}

// --- 6. 複数次元テーブル共存下の実行（TABLE-2 関連） ------------------------------

#[test]
fn arbitrary_tables_with_distinct_dims_each_return_only_own_rows() {
    let path = unique_db_path("arb-multi-dim-coexist");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("kb_small", 3))
        .expect("create kb_small");
    storage
        .create_table(&schema("kb_large", 8))
        .expect("create kb_large");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "kb_small",
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0, 0.0]),
            Value::Text("small-body".to_string()),
            Value::Text("en".to_string()),
        ],
        &op("dim-seed-small"),
    )
    .expect("insert into kb_small");
    engine::tenant::insert_typed_row(
        &storage,
        "kb_large",
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            Value::Text("large-body".to_string()),
            Value::Text("en".to_string()),
        ],
        &op("dim-seed-large"),
    )
    .expect("insert into kb_large");

    let core = new_core(storage);
    let read_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let small = core
        .execute_sql(
            &read_ctx,
            "SELECT * FROM kb_small ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 5",
        )
        .expect("kb_small C1 should succeed");
    assert_eq!(result_ids(&small), vec![1]);
    assert_eq!(small.rows[0].cells[2], Cell::Text("small-body".to_string()));

    let large = core
        .execute_sql(
            &read_ctx,
            "SELECT * FROM kb_large ORDER BY embedding <=> '[1.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]' LIMIT 5",
        )
        .expect("kb_large C1 should succeed");
    assert_eq!(result_ids(&large), vec![1]);
    assert_eq!(large.rows[0].cells[2], Cell::Text("large-body".to_string()));

    // 次元不一致のベクトルリテラルは fail-closed に拒否される（`sql/parser.rs`
    // の `bind_query`／`bind_insert` が検証する `22000`。`sql_surface.rs`・
    // `sql/parser.rs` 内のユニットテスト群と同一の wire_code）。
    let err = core
        .execute_sql(
            &read_ctx,
            "SELECT * FROM kb_small ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5",
        )
        .expect_err("dimension mismatch must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- 7. 境界の対照（SQL-8 側の拒否契約） ------------------------------------------

#[test]
fn select_from_unknown_table_is_rejected_with_42p01() {
    let path = unique_db_path("arb-unknown-table");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema("kb_articles", 2))
        .expect("create table");
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_sql(
            &ctx,
            "SELECT * FROM nonexistent_table ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5",
        )
        .expect_err("unknown table must be rejected");
    assert_eq!(err.wire_code(), "42P01");
}
