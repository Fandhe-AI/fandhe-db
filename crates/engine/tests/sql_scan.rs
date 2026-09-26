//! 広域取得（ソートなしのフィルタ取得。`SELECT ... [WHERE ...] LIMIT n`）の結合
//! テスト（Issue #454）。ポインタ: `docs/design/wide-retrieval-scan.md`（spec
//! ビヘイビア ID は SQL-15・TASK-170 として付与済み〔vector-db-spec#12〕。
//! 確定化は TASK-170 が担う。本モジュールは本リポの実装既定値の契約を固定する）。
//!
//! `tests/sql_aggregate.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、決定的
//! 擬似乱数 xorshift64*、production の判定関数（`PolicyContext::is_visible`）を
//! 呼ばない独立オラクル）で検証する。テナント境界（RLS-8）の一般化検証は
//! `tests/rls_generalized.rs` の `scan_*` テストが別途担う（本ファイルは実行契約
//! ――早期終了・投影種別・`LIMIT` 範囲・取得モード非依存・`EXPLAIN` 拒否――に
//! 焦点を絞る）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f32_signed(&mut self) -> f32 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        (unit * 2.0 - 1.0) as f32
    }
}

const DIM: usize = 4;
const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 単一テナント・全件 `Public` のコーパスを構築する（早期終了・投影種別・`LIMIT`
/// 範囲の検証はテナント境界とは独立な軸のため、`tests/rls_generalized.rs` の
/// 多テナントフィクスチャとは別に単純なものを使う）。
fn seed_single_tenant_corpus(storage: &Storage, tenant: &str, n: u64) -> Vec<(u64, &'static str)> {
    storage.create_table(&schema()).expect("create table");
    let mut rng = Xorshift64::new(0x5CA1_AB1E);
    let mut truths = Vec::new();
    const LANGS: [&str; 2] = ["ja", "en"];
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    for id in 1..=n {
        let lang = LANGS[(id as usize) % LANGS.len()];
        let emb: Vec<f32> = (0..DIM).map(|_| rng.next_f32_signed()).collect();
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb), Value::Text(lang.to_string())],
            &op_id,
        )
        .expect("insert row");
        truths.push((id, lang));
    }
    truths
}

fn expect_query(outcome: SqlOutcome) -> engine::sql::exec::QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn result_ids(result: &engine::sql::exec::QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

// ---------- 基本受理・早期終了 ----------

#[test]
fn scan_returns_at_most_limit_rows_all_matching_where() {
    let path = unique_db_path("scan-basic");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_single_tenant_corpus(&storage, "tenant-a", 20);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(&ctx, "SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 5")
        .expect("scan should succeed");
    assert!(result.rows.len() <= 5);
    let ja_ids: std::collections::HashSet<u64> = truths
        .iter()
        .filter(|(_, lang)| *lang == "ja")
        .map(|(id, _)| *id)
        .collect();
    for row in &result.rows {
        assert!(
            ja_ids.contains(&row.id),
            "row {} does not match WHERE lang = 'ja'",
            row.id
        );
    }
}

#[test]
fn scan_limit_at_total_returns_every_matching_row_exactly_once() {
    let path = unique_db_path("scan-full");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_single_tenant_corpus(&storage, "tenant-a", 12);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(&ctx, "SELECT id FROM docs LIMIT 10000")
        .expect("scan should succeed");
    let got: std::collections::HashSet<u64> = result_ids(&result).into_iter().collect();
    let expected: std::collections::HashSet<u64> = truths.iter().map(|(id, _)| *id).collect();
    assert_eq!(got, expected);
    assert_eq!(result.rows.len(), truths.len());
}

// ---------- 決定性 ----------

#[test]
fn scan_result_order_is_deterministic_across_repeated_calls() {
    let path = unique_db_path("scan-determinism");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_single_tenant_corpus(&storage, "tenant-a", 30);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let first = result_ids(&expect_query(
        core.execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            "SELECT id FROM docs LIMIT 8",
        )
        .expect("first call should succeed"),
    ));
    let second = result_ids(&expect_query(
        core.execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            "SELECT id FROM docs LIMIT 8",
        )
        .expect("second call should succeed"),
    ));
    assert_eq!(first, second);
}

// ---------- 投影種別 ----------

#[test]
fn scan_star_projection_includes_id_vector_and_text_cells() {
    let path = unique_db_path("scan-star");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_single_tenant_corpus(&storage, "tenant-a", 3);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(&ctx, "SELECT * FROM docs LIMIT 10")
        .expect("scan should succeed");
    assert_eq!(result.columns.len(), 3);
    assert!(matches!(result.columns[0], ColumnMeta::Id));
    assert!(matches!(
        result.columns[1],
        ColumnMeta::Scalar { ty: ColumnType::Vector(d), .. } if d == DIM as u32
    ));
    assert!(matches!(
        result.columns[2],
        ColumnMeta::Scalar {
            ty: ColumnType::Text,
            ..
        }
    ));
    assert_eq!(result.rows.len(), 3);
    for row in &result.rows {
        assert!(matches!(row.cells[0], Cell::Integer(_)));
        assert!(matches!(row.cells[1], Cell::Vector(ref v) if v.len() == DIM));
        assert!(matches!(row.cells[2], Cell::Text(_)));
    }
}

#[test]
fn scan_over_declared_udf_call_evaluates_computed_column() {
    let path = unique_db_path("scan-udf");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_single_tenant_corpus(&storage, "tenant-a", 3);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION n(v) AS vec_norm(v)")
        .expect("CREATE FUNCTION should succeed");
    let result = expect_query(
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, n(embedding) FROM docs LIMIT 10",
        )
        .expect("scan with computed column should succeed"),
    );
    assert!(matches!(result.columns[1], ColumnMeta::Computed { .. }));
    for row in &result.rows {
        match row.cells[1] {
            Cell::Float(v) => assert!(v.is_finite() && v >= 0.0),
            ref other => panic!("expected Cell::Float, got {other:?}"),
        }
    }

    // 別セッション（UDF `n` 未登録）では未知の関数として `22000` になる。
    let err = core
        .execute_sql(&ctx, "SELECT n(embedding) FROM docs LIMIT 10")
        .expect_err("undefined UDF must be rejected in a fresh session");
    assert_eq!(err.wire_code(), "22000");
}

// `VECTOR` 列が未設定（NULL）の既存行に対する `Cell::Null` 投影は、低レベル
// `storage`/`catalog` API（`pub(crate)`）を要するため `sql::scan` モジュール内の
// 単体テスト（`crate::sql::scan::tests::projects_null_for_unset_nullable_vector_column`）
// として固定する（`sql::aggregate` モジュール内テストの `write_row_direct` と同じ
// 制約・同じ手法）。ここ（クレート外の結合テスト）からは公開 API のみで到達できる
// 契約のみを検証する。

// ---------- 空テーブル・未作成行テーブル ----------

#[test]
fn scan_against_table_with_no_rows_returns_empty_result_with_columns() {
    let path = unique_db_path("scan-empty");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    // 行を一切書き込まない: 行テーブル自体が未作成の状態
    // （`redb::TableError::TableDoesNotExist`）を再現する。
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let result = core
        .execute_sql(&ctx, "SELECT * FROM docs LIMIT 10")
        .expect("scan against an empty table should succeed");
    assert_eq!(result.rows.len(), 0);
    assert_eq!(result.columns.len(), 3);
}

// ---------- LIMIT 範囲検証 ----------

#[test]
fn scan_rejects_limit_zero() {
    let path = unique_db_path("scan-limit-zero");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_single_tenant_corpus(&storage, "tenant-a", 3);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let err = core
        .execute_sql(&ctx, "SELECT * FROM docs LIMIT 0")
        .expect_err("LIMIT 0 must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn scan_rejects_limit_exceeding_max_search_k() {
    let path = unique_db_path("scan-limit-over");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_single_tenant_corpus(&storage, "tenant-a", 3);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let err = core
        .execute_sql(&ctx, "SELECT * FROM docs LIMIT 10001")
        .expect_err("LIMIT above MAX_SEARCH_K must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn scan_accepts_limit_exactly_max_search_k() {
    let path = unique_db_path("scan-limit-max");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_single_tenant_corpus(&storage, "tenant-a", 3);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    core.execute_sql(&ctx, "SELECT * FROM docs LIMIT 10000")
        .expect("LIMIT at MAX_SEARCH_K must be accepted");
}

// ---------- 取得モードからの独立性 ----------

#[test]
fn scan_result_is_unaffected_by_session_search_mode() {
    let path = unique_db_path("scan-mode-independent");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_single_tenant_corpus(&storage, "tenant-a", 10);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let mut session = SessionState::default();
    let recall_result = result_ids(&expect_query(
        core.execute_sql_in_session(&ctx, &mut session, "SELECT id FROM docs LIMIT 6")
            .expect("scan under default mode should succeed"),
    ));

    core.execute_sql_in_session(&ctx, &mut session, "SET search_mode = 'precision'")
        .expect("SET search_mode should succeed");
    let precision_result = result_ids(&expect_query(
        core.execute_sql_in_session(&ctx, &mut session, "SELECT id FROM docs LIMIT 6")
            .expect("scan under precision mode should succeed"),
    ));

    assert_eq!(recall_result, precision_result);
}

// ---------- EXPLAIN 前置の受理（Issue #922・SQL-27） ----------

#[test]
fn explain_accepts_bare_limit_scan() {
    // Issue #922（SQL-27）: `EXPLAIN` の対象を広域取得へ拡大したため、`USING
    // PLAN` を伴わない bare LIMIT scan はもはや拒否されず受理される（受理
    // テストへ反転）。`sql::scan` はランキング段・索引を持たないため
    // `scalar_plan: plain_scan`／`access_path: full_scan` に固定。
    let path = unique_db_path("scan-explain-accept");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_single_tenant_corpus(&storage, "tenant-a", 3);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a");

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, "EXPLAIN SELECT * FROM docs LIMIT 10")
        .expect("EXPLAIN over a bare LIMIT scan must be accepted");
    match outcome {
        SqlOutcome::Explain(result) => {
            let lines: Vec<String> = result
                .rows
                .iter()
                .map(|row| match &row.cells[0] {
                    Cell::Text(s) => s.clone(),
                    other => panic!("expected Cell::Text, got {other:?}"),
                })
                .collect();
            assert_eq!(
                lines,
                vec!["scalar_plan: plain_scan", "access_path: full_scan"]
            );
        }
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}
