//! `engine::core::EngineCore::execute_sql_in_session` の結合テスト（TASK-161、対象
//! ビヘイビア: SQL-12。ポインタ: `docs/spec/05-tasks.md` TASK-161・
//! `docs/spec/04-behavior/sql-surface.md` SQL-12）。
//!
//! `tests/sql_surface.rs`・`tests/sql_allowlist.rs` と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`）で、`USING MODE` 句・
//! `SET search_mode`・優先順位解決（クエリ句 > セッション変数 > 既定）・拒否経路・
//! セッション分離を検証する。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::mode::{SearchMode, SessionState};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "fandhe-db-engine-sql-search-mode-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

struct CleanupGuard(PathBuf);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `docs` テーブル（`embedding VECTOR(3)`）を持つ `EngineCore` を新設し、
/// 決定的な小規模コーパスを投入する（`tests/sql_surface.rs` の SQL-1 テストと
/// 同一の投入手順）。
fn new_core_with_docs() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("docs");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let corpus: Vec<(u64, [f32; 3])> = vec![
        (1, [1.0, 0.0, 0.0]),
        (2, [0.9, 0.1, 0.0]),
        (3, [0.0, 1.0, 0.0]),
    ];
    for (id, emb) in &corpus {
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
            &storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &op_id,
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, guard)
}

const SELECT_NO_MODE: &str = "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3";

// --- 1. 既定モード（句なし・SET なし） -------------------------------------------

#[test]
fn default_mode_recall_matches_pre_existing_sql1_behavior() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, SELECT_NO_MODE)
        .expect("default recall execution should succeed");
    match outcome {
        SqlOutcome::Query(result) => {
            // 独立オラクル: f64 で総当たり内積を計算し、スコア降順・同点 id 昇順で
            // Top-3 を選ぶ（`tests/sql_surface.rs` の SQL-1 と同一オラクル。既定
            // モード `recall` の挙動が SQL-1 から不変であることを確認する）。
            let query = [1.0f64, 0.0, 0.0];
            let corpus: Vec<(u64, [f64; 3])> = vec![
                (1, [1.0, 0.0, 0.0]),
                (2, [0.9, 0.1, 0.0]),
                (3, [0.0, 1.0, 0.0]),
            ];
            let mut scored: Vec<(u64, f64)> = corpus
                .iter()
                .map(|(id, emb)| {
                    let dot: f64 = emb.iter().zip(query.iter()).map(|(a, b)| a * b).sum();
                    (*id, dot)
                })
                .collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let expected: Vec<u64> = scored.into_iter().map(|(id, _)| id).collect();
            let actual: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
            assert_eq!(actual, expected);
        }
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

// --- 2〜3. SET search_mode ------------------------------------------------------

#[test]
fn set_search_mode_recall_then_select_resolves_to_session_variable() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, "SET search_mode = 'recall'")
        .expect("SET search_mode = 'recall' should succeed");
    assert_eq!(outcome, SqlOutcome::SetSearchMode(SearchMode::Recall));
    assert_eq!(session.search_mode(), Some(SearchMode::Recall));

    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, SELECT_NO_MODE)
        .expect("recall session variable should execute");
    assert!(matches!(outcome, SqlOutcome::Query(_)));
}

#[test]
fn set_search_mode_precision_gates_select_execution_to_confidence_filtered_results() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, "SET search_mode = 'precision'")
        .expect("SET search_mode = 'precision' should succeed");
    assert_eq!(outcome, SqlOutcome::SetSearchMode(SearchMode::Precision));

    // TASK-162（SEARCH-9）: `precision` は候補生成（`recall` と共通）の結果へ確信度
    // 判定を適用する。本コーパス（cosine(query, id1)=1.0／cosine(query, id2)≈0.994／
    // cosine(query, id3)=0.0）は Top-1・Top-2 のマージンが既定閾値（0.05）未満のため
    // 空集合（0 行）の**通常応答**として返る（エラーではない。fail-closed）。
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, SELECT_NO_MODE)
        .expect("precision execution must succeed with an empty result, not an error");
    match outcome {
        SqlOutcome::Query(result) => assert!(result.rows.is_empty()),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

// --- 4〜5. クエリ句とセッション変数の優先順位 -------------------------------------

#[test]
fn query_clause_recall_wins_over_session_precision() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, "SET search_mode = 'precision'")
        .expect("SET should succeed");

    let sql =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE 'recall'";
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect("query clause 'recall' must win over session 'precision'");
    assert!(matches!(outcome, SqlOutcome::Query(_)));
}

#[test]
fn query_clause_precision_wins_over_session_recall_and_applies_confidence_gate() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, "SET search_mode = 'recall'")
        .expect("SET should succeed");

    // `USING MODE 'precision'` がセッション変数 `recall` に優先し（TASK-161・SQL-12
    // が担う優先順位解決）、実行契約（TASK-162・SEARCH-9）が適用される。本コーパスは
    // マージン不足のため空集合（0 行）の通常応答になる（`recall` へ黙ってフォール
    // バックしない。フォールバックすると 3 行返り、この検査で区別できる）。
    let sql =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE 'precision'";
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect("query clause 'precision' must win over session 'recall' and succeed");
    match outcome {
        SqlOutcome::Query(result) => assert!(result.rows.is_empty()),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

// --- 6. 失敗した SET はセッションを変更しない -------------------------------------

#[test]
fn failed_set_search_mode_leaves_session_unchanged() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, "SET search_mode = 'recall'")
        .expect("SET should succeed");
    assert_eq!(session.search_mode(), Some(SearchMode::Recall));

    let err = core
        .execute_sql_in_session(&ctx, &mut session, "SET search_mode = 'fuzzy'")
        .expect_err("unknown mode value must be rejected");
    assert_eq!(err.wire_code(), "22000");
    // 失敗した SET は直前のセッション値のまま（部分更新＝黙った既定化にしない）。
    assert_eq!(session.search_mode(), Some(SearchMode::Recall));
}

// --- 7. USING MODE の拒否経路 -----------------------------------------------------

#[test]
fn using_mode_rejects_unknown_and_empty_literal_values() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    for literal in ["FUZZY", ""] {
        let sql = format!(
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE '{literal}'"
        );
        let err = core
            .execute_sql_in_session(&ctx, &mut session, &sql)
            .expect_err("unknown/empty mode literal must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }
}

#[test]
fn using_mode_rejects_dollar_parameter_and_bare_identifier_forms() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let dollar_form =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE $1";
    let err = core
        .execute_sql_in_session(&ctx, &mut session, dollar_form)
        .expect_err("$n parameter form must be rejected in MVP");
    assert_eq!(err.wire_code(), "42601");

    let ident_form =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE recall";
    let err = core
        .execute_sql_in_session(&ctx, &mut session, ident_form)
        .expect_err("unquoted mode value must be rejected as a syntax error");
    assert_eq!(err.wire_code(), "42601");
}

// --- 8. セッション分離 -------------------------------------------------------------

#[test]
fn two_sessions_do_not_leak_search_mode_into_each_other() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session_a = SessionState::default();
    let mut session_b = SessionState::default();

    core.execute_sql_in_session(&ctx, &mut session_a, "SET search_mode = 'precision'")
        .expect("SET on session_a should succeed");

    assert_eq!(session_a.search_mode(), Some(SearchMode::Precision));
    assert_eq!(session_b.search_mode(), None);

    // session_b は既定 `recall` のまま実行できる（session_a の precision が波及しない）。
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session_b, SELECT_NO_MODE)
        .expect("session_b should still resolve to default recall");
    assert!(matches!(outcome, SqlOutcome::Query(_)));
}

// --- 9. セッションを持たない既存エントリポイント -----------------------------------

#[test]
fn stateless_execute_sql_rejects_set_search_mode() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_sql(&ctx, "SET search_mode = 'recall'")
        .expect_err("SET must be rejected on the session-less entry point, not silently no-op'd");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn stateless_execute_sql_rejects_set_search_mode_with_42601_regardless_of_literal_validity() {
    // codex-review P1 指摘対応: `execute_sql` は statement 種別（SET か SELECT か）のみで
    // 拒否を判定し、`SET` のリテラル値が妥当（`recall`／`precision`）か無効（`fuzzy` 等）
    // かに関わらず同じ `42601` を返す。以前は内部で `execute_sql_in_session` へ委譲して
    // いたため、リテラル値の妥当性検証（`SearchMode::parse_literal`）が先に走り、無効な
    // 値だけ `22000` を返す非決定的な契約になっていた（同じ「非対応 statement」のはずが
    // 値によってエラーコードが変わっていた）。
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let valid_literal_err = core
        .execute_sql(&ctx, "SET search_mode = 'recall'")
        .expect_err("SET with a valid literal must still be rejected on this entry point");
    let invalid_literal_err = core
        .execute_sql(&ctx, "SET search_mode = 'fuzzy'")
        .expect_err("SET with an invalid literal must also be rejected on this entry point");

    assert_eq!(valid_literal_err.wire_code(), "42601");
    assert_eq!(invalid_literal_err.wire_code(), "42601");
}

#[test]
fn stateless_execute_sql_still_resolves_default_recall_mode() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_sql(&ctx, SELECT_NO_MODE)
        .expect("plain SELECT via the legacy entry point must keep working (default recall)");
}

// --- 10. 決定性 ---------------------------------------------------------------------

#[test]
fn same_input_yields_same_result_across_repeated_calls() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let sql =
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3 USING MODE 'precision'";

    let extract_ids = |core: &EngineCore, session: &mut SessionState| -> Vec<u64> {
        match core
            .execute_sql_in_session(&ctx, session, sql)
            .expect("precision execution should succeed")
        {
            SqlOutcome::Query(result) => result.rows.iter().map(|r| r.id).collect(),
            other => panic!("expected SqlOutcome::Query, got {other:?}"),
        }
    };

    let first = extract_ids(&core, &mut session);
    let second = extract_ids(&core, &mut session);
    assert_eq!(first, second);
}
