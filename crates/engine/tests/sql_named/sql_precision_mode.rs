//! `precision` モードの実行契約（TASK-162、対象ビヘイビア: SEARCH-9。ポインタ:
//! `docs/spec/05-tasks.md` TASK-162・`docs/spec/04-behavior/search.md` SEARCH-9）の
//! 結合テスト。
//!
//! `tests/sql_search_mode.rs`・`tests/sql_surface.rs` と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`tenant::insert_typed_row`）で、
//! 確信度判定（dense の cosine 類似度・hybrid の正規化 RRF）・`HINT ORDER` との
//! 整合・テナント境界・fail-open 経路の不在を検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::precision::PrecisionPolicy;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
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

/// `SqlOutcome::Query` を期待するアサーションヘルパ（`SqlOutcome` は `Debug` の
/// みで `unwrap` 系ヘルパを持たないため、テストごとの重複を避けて共通化する）。
fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

// --- 1〜4: dense（cosine 類似度） ---------------------------------------------------

#[test]
fn dense_clear_winner_returns_top1_only_while_recall_returns_all() {
    let path = unique_db_path("precision-dense-clear-winner");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    for (id, emb) in [
        (1u64, [1.0f32, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0]),
        (3, [0.0, 0.0, 1.0]),
    ] {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let precision_sql =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE 'precision'";
    let precision_result = core
        .execute_sql(&ctx, precision_sql)
        .expect("precision execution should succeed");
    assert_eq!(result_ids(&precision_result), vec![1]);

    let recall_sql =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE 'recall'";
    let recall_result = core
        .execute_sql(&ctx, recall_sql)
        .expect("recall execution should succeed");
    assert_eq!(result_ids(&recall_result).len(), 3);
}

#[test]
fn dense_ambiguous_top1_and_top2_returns_empty_result() {
    let path = unique_db_path("precision-dense-ambiguous");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    // cosine(query, id1) = 1.0、cosine(query, id2) = 0.99/sqrt(0.99^2+0.1^2) ≈ 0.9950。
    // 差は既定マージン閾値 0.05 未満のため空集合になる。
    for (id, emb) in [(1u64, [1.0f32, 0.0, 0.0]), (2, [0.99, 0.1, 0.0])] {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE 'precision'";
    let result = expect_query(
        core.execute_sql_in_session(&ctx, &mut SessionState::default(), sql)
            .expect("precision execution must succeed with an empty result, not an error"),
    );
    assert!(result.rows.is_empty());
    // 空集合でも投影メタは通常どおり構成される（エラー応答ではないことの傍証。
    // `SELECT *` は `id` 疑似列＋`embedding` 列の 2 列を投影する）。
    assert_eq!(result.columns.len(), 2);
}

#[test]
fn dense_low_similarity_returns_empty_result() {
    let path = unique_db_path("precision-dense-low-sim");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    // クエリ [1,0,0] と全行が直交する（cosine 0.0）。
    for (id, emb) in [(1u64, [0.0f32, 1.0, 0.0]), (2, [0.0, 0.0, 1.0])] {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE 'precision'";
    let result = core.execute_sql(&ctx, sql).expect("precision must succeed");
    assert!(result.rows.is_empty());
}

#[test]
fn dense_clear_winner_result_never_exceeds_max_results_regardless_of_limit() {
    let path = unique_db_path("precision-dense-limit5");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    for (id, emb) in [
        (1u64, [1.0f32, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0]),
        (3, [0.0, 0.0, 1.0]),
    ] {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    // `LIMIT 5` を指定しても、既定 `PrecisionPolicy::max_results()`（1）を超えない。
    let sql =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 5 USING MODE 'precision'";
    let result = core.execute_sql(&ctx, sql).expect("precision must succeed");
    assert!(result.rows.len() <= 1);
    assert_eq!(result_ids(&result), vec![1]);
}

// --- 5: SET と USING MODE の実行契約の同一性 -----------------------------------------

#[test]
fn set_search_mode_and_using_mode_yield_identical_precision_results() {
    let path = unique_db_path("precision-set-vs-using");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    for (id, emb) in [
        (1u64, [1.0f32, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0]),
        (3, [0.0, 0.0, 1.0]),
    ] {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let mut session_via_set = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session_via_set, "SET search_mode = 'precision'")
        .expect("SET should succeed");
    let via_set = expect_query(
        core.execute_sql_in_session(
            &ctx,
            &mut session_via_set,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
        )
        .expect("SET-driven precision execution should succeed"),
    );

    let via_using_mode = expect_query(
        core.execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE 'precision'",
        )
        .expect("USING MODE precision execution should succeed"),
    );

    assert_eq!(result_ids(&via_set), result_ids(&via_using_mode));
}

// --- 6〜8: hybrid（正規化 RRF） ------------------------------------------------------

#[test]
fn hybrid_dense_and_sparse_agree_on_rank1_returns_top1_only() {
    let path = unique_db_path("precision-hybrid-agree");
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
    // 密・疎ともに id1 が 1 位、id2 が 2 位で一致する（本文の "dog" 出現頻度で疎の
    // 順位を制御。長さは揃えて length normalization の影響を避ける）。
    let rows: [(u64, [f32; 2], &str); 2] = [
        (1, [1.0, 0.0], "dog dog dog cat"),
        (2, [0.0, 1.0], "dog cat cat cat"),
    ];
    for (id, emb, body) in rows {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(body.to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql = "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'dog') \
               LIMIT 2 USING MODE 'precision'";
    let result = core
        .execute_sql(&ctx, sql)
        .expect("precision hybrid must succeed");
    assert_eq!(result_ids(&result), vec![1]);
}

#[test]
fn hybrid_dense_and_sparse_disagree_returns_empty_result() {
    let path = unique_db_path("precision-hybrid-disagree");
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
    // 密は id1 を 1 位に置くが（クエリベクトル [1,0]）、疎は "dog" の出現頻度により
    // id2 を 1 位に置く（対称的な食い違い）。RRF 融合スコアが完全に同点になり、
    // マージン 0 として空集合へ倒れる。
    let rows: [(u64, [f32; 2], &str); 2] = [
        (1, [1.0, 0.0], "dog cat cat cat"),
        (2, [0.0, 1.0], "dog dog dog cat"),
    ];
    for (id, emb, body) in rows {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(body.to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql = "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'dog') \
               LIMIT 2 USING MODE 'precision'";
    let result = core
        .execute_sql(&ctx, sql)
        .expect("precision hybrid must succeed");
    assert!(result.rows.is_empty());
}

#[test]
fn hybrid_dense_only_degradation_returns_empty_result() {
    let path = unique_db_path("precision-hybrid-dense-degrade");
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
    // 可視行に本文を持つ行が 1 件もないため密のみへ縮退する
    // （`tests/sql_surface.rs` の `sql4_hybrid_degrades_to_dense_only_...` と同一構成）。
    for (id, emb) in [(1u64, [1.0f32, 0.0]), (2, [0.0, 1.0])] {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
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
    let sql = "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'anything') \
               LIMIT 2 USING MODE 'precision'";
    let result = core
        .execute_sql(&ctx, sql)
        .expect("precision hybrid must succeed");
    // 密のみへ縮退した場合、単一検索器のみの寄与では正規化 RRF スコアの理論最大値
    // の半分にしかならず、既定 hybrid 閾値（0.98）に届かないため意図的に空集合。
    assert!(result.rows.is_empty());
}

// --- 9: HINT ORDER（DISTANCE 先行）との整合 -----------------------------------------

#[test]
fn precision_gate_applies_after_distance_first_scalar_postfilter() {
    let path = unique_db_path("precision-hint-order");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    // dense の Top-1（id1）は `lang = 'ja'` を満たさない。DISTANCE 先行
    // （`HINT ORDER(DISTANCE, SCALAR, RLS)`）では候補構築時に等価条件を適用せず、
    // DISTANCE 段の後で事後適用するため、ゲートは SCALAR 事後フィルタ**後**の
    // 順位列（id1 除去済み）に対して働く。
    let rows: [(u64, [f32; 3], &str); 2] =
        [(1, [1.0, 0.0, 0.0], "en"), (2, [0.99, 0.1, 0.0], "ja")];
    for (id, emb, lang) in rows {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
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
    let sql = "SELECT * FROM docs WHERE lang = 'ja' \
               ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1 \
               HINT ORDER(DISTANCE, SCALAR, RLS) USING MODE 'precision'";
    let result = core.execute_sql(&ctx, sql).expect("precision must succeed");
    assert_eq!(result_ids(&result), vec![2]);
}

#[test]
fn precision_gate_fail_closed_when_postfilter_survivors_exceed_limit_based_k_eff() {
    // codex-review / Bugbot 指摘の回帰テスト: SCALAR 事後フィルタ（DISTANCE 先行）
    // 経路で、WHERE を満たす Top-2 が `bound.limit`（ここでは `LIMIT 1`）由来の
    // 狭い取得件数の外側に位置するケース。修正前は DISTANCE 段が
    // `bound.limit.max(2) == 2` 件しか取得せず、WHERE 不一致の Top-1（id1）を
    // 除去した後に残る候補が id2 の 1 件のみになり、「Top-2 が存在しない」＝
    // マージン条件成立と誤判定して fail-open に id2 を返していた
    // （id2・id3 の確信度差は既定マージン閾値 0.05 未満で本来は空集合が正しい）。
    // 事後フィルタ経路の `k_eff` を可視集合全体まで広げた修正後は、WHERE を
    // 満たす完全な順位列（id2, id3）に対してマージン判定が行われ、空集合になる。
    let path = unique_db_path("precision-hint-order-beyond-k-eff");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    // id1: dense Top-1（内積 1.0）だが lang != 'ja' → SCALAR 事後フィルタで除去。
    // id2: dense Top-2（内積 0.99）・lang = 'ja'。
    // id3: dense Top-3（内積 0.97）・lang = 'ja'。id2 との cosine 類似度差が
    // 既定マージン閾値（0.05）未満になるよう選んだベクトル。
    let rows: [(u64, [f32; 3], &str); 3] = [
        (1, [1.0, 0.0, 0.0], "en"),
        (2, [0.99, 0.1, 0.0], "ja"),
        (3, [0.97, 0.2, 0.0], "ja"),
    ];
    for (id, emb, lang) in rows {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
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
    let sql = "SELECT * FROM docs WHERE lang = 'ja' \
               ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1 \
               HINT ORDER(DISTANCE, SCALAR, RLS) USING MODE 'precision'";
    let result = core.execute_sql(&ctx, sql).expect("precision must succeed");
    assert!(result.rows.is_empty());
}

// --- 10: fail-open 経路の不在（外部入力） -------------------------------------------

#[test]
fn no_external_input_can_disable_or_relax_the_confidence_gate() {
    let path = unique_db_path("precision-no-external-fail-open");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 0.0, 0.0])],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert row");
    let core = new_core(storage);

    // クエリ単位でも `SET` 単位でも、閾値・確信度判定を無効化する句・変数名は
    // 構文として存在しない。いずれも `42601`（未対応構文）として拒否される
    // （リテラル値の妥当性検証まで到達しない＝そもそも受理経路がないことを示す）。
    let rejected_query_forms = [
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1 \
         USING MODE 'precision' USING THRESHOLD 0",
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1 \
         USING MODE 'precision' USING THRESHOLD 0.0",
    ];
    for sql in rejected_query_forms {
        let err = core
            .execute_sql(&ctx, sql)
            .expect_err("no query-level threshold override syntax must exist");
        assert_eq!(err.wire_code(), "42601");
    }

    let rejected_set_forms = [
        "SET precision_threshold = '0'",
        "SET search_mode_min_top1 = '0'",
        "SET precision_min_margin = '0'",
    ];
    for sql in rejected_set_forms {
        let err = core
            .execute_sql_in_session(&ctx, &mut SessionState::default(), sql)
            .expect_err("no session variable can relax the precision gate");
        assert_eq!(err.wire_code(), "42601");
    }

    // `SET search_mode = 'precision'` 自体は受理されるが、後続 SELECT の確信度判定
    // 自体を変更する経路にはならない（`crate::precision::PrecisionPolicy` はサーバー
    // 側専有）。
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, "SET search_mode = 'precision'")
        .expect("SET search_mode = 'precision' should succeed");
}

// --- 11: fail-open 経路の不在（設定値） ---------------------------------------------

#[test]
fn precision_policy_new_rejects_zero_negative_and_nan_thresholds() {
    assert!(PrecisionPolicy::new(0.0, 0.05, 0.98, 0.005, 1).is_err());
    assert!(PrecisionPolicy::new(0.8, -0.05, 0.98, 0.005, 1).is_err());
    assert!(PrecisionPolicy::new(f64::NAN, 0.05, 0.98, 0.005, 1).is_err());
    assert!(PrecisionPolicy::new(0.8, 0.05, 0.0, 0.005, 1).is_err());
}

#[test]
fn precision_result_depends_only_on_server_side_policy_not_query_input() {
    let path = unique_db_path("precision-server-side-policy");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    for (id, emb) in [
        (1u64, [1.0f32, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0]),
        (3, [0.0, 0.0, 1.0]),
    ] {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    // 既定ポリシーでは 1 行（id=1）を返す構成（`dense_clear_winner_...` と同一
    // コーパス）だが、`with_precision_policy` でマージン閾値を極端に厳しくすると
    // （実際のマージンは 1.0）空集合へ倒れる。判定がサーバー側ポリシーにのみ依存
    // し、クエリ・セッションからは変更できないことを示す。
    let strict_policy = PrecisionPolicy::new(0.8, 1.5, 0.98, 0.005, 1)
        .expect("strict-but-valid policy must construct");
    let core = new_core(storage).with_precision_policy(strict_policy);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE 'precision'";
    let result = core.execute_sql(&ctx, sql).expect("precision must succeed");
    assert!(result.rows.is_empty());
}

// --- 12: テナント境界 ----------------------------------------------------------------

#[test]
fn tenant_a_precision_result_is_unaffected_by_tenant_b_private_row_existence() {
    // tenant-b の `Private` 行がクエリと同一ベクトルであっても、tenant-a の
    // `precision` 結果はその行が存在しない場合と同一になる（結果行数・エラーの
    // 有無から他テナント行の存在が推定できない。security.md「テナント境界」）。
    let run = |insert_tenant_b_exact_match: bool| -> Vec<u64> {
        let path = unique_db_path(&format!(
            "precision-tenant-boundary-{insert_tenant_b_exact_match}"
        ));
        let _guard = CleanupGuard(path.clone());
        let storage = open_storage(&path);
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
            ))
            .expect("create table");
        // tenant-a 自身の行（クエリとの cosine ≈ 0.7071。既定 dense 閾値 0.8 未満）。
        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx_a,
            1,
            Visibility::Public,
            &[Value::Vector(vec![0.5, 0.5, 0.0])],
            &engine::recovery::required_op_id::OperationId::parse("test-op-tenant-a")
                .expect("valid operation_id"),
        )
        .expect("insert tenant-a row");
        if insert_tenant_b_exact_match {
            let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
            engine::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx_b,
                2,
                Visibility::Private,
                &[Value::Vector(vec![1.0, 0.0, 0.0])],
                &engine::recovery::required_op_id::OperationId::parse("test-op")
                    .expect("valid operation_id"),
            )
            .expect("insert tenant-b row");
        }
        let core = new_core(storage);
        let sql = "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 \
                   USING MODE 'precision'";
        let result = core
            .execute_sql(&ctx_a, sql)
            .expect("precision must succeed from tenant-a's perspective");
        result_ids(&result)
    };

    let without_tenant_b_row = run(false);
    let with_tenant_b_row = run(true);
    assert_eq!(without_tenant_b_row, with_tenant_b_row);
    assert!(without_tenant_b_row.is_empty());
}
