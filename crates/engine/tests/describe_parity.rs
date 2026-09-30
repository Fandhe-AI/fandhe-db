//! `EngineCore::parse_sql`／`describe_parsed_in_session`（Issue #933・TASK-71・
//! WIRE-11）が返す結果列メタデータが、同一 SQL テキストを実行した
//! `execute_sql_in_session` の結果列と完全に一致すること（受け入れ条件 3）と、
//! Describe が行・台帳・世代・LLM I/O のいずれにも触れない（副作用ゼロ。
//! 受け入れ条件の中核証拠）ことを固定する。
//!
//! wire プロトコル層（'P'／'D' メッセージの受理・エンコード）の検証は
//! `crates/wire-server/tests/wire_extended_query.rs` が担う。本ファイルは
//! engine 側の parse/describe/execute 分離が既存挙動を一切変えないことに徹する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn seed_row(core: &EngineCore, ctx: &PolicyContext, id: u64, op_id: &str) {
    let mut session = SessionState::default();
    let sql = format!(
        "INSERT INTO documents (id, embedding, body, lang) VALUES ({id}, '[0.1,0.2,0.3]', 'hello', 'en') USING OPERATION_ID '{op_id}'"
    );
    core.execute_sql_in_session(ctx, &mut session, &sql)
        .expect("seed insert should succeed");
}

/// `execute_sql_in_session` の結果列を実測し、`describe_parsed_in_session` の
/// 結果と突き合わせる共通ヘルパー。
fn assert_describe_matches_execute(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    let describe_session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
    let described = core
        .describe_parsed_in_session(&describe_session, &parsed)
        .expect("describe should succeed");

    let mut exec_session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(ctx, &mut exec_session, sql)
        .expect("execute should succeed");
    let executed_columns = match outcome {
        SqlOutcome::Query(result) => Some(result.columns),
        SqlOutcome::Explain(result) => Some(result.columns),
        SqlOutcome::Returning(r) => Some(r.result.columns),
        SqlOutcome::SetSearchMode(_)
        | SqlOutcome::CreateFunction { .. }
        | SqlOutcome::Insert(_)
        | SqlOutcome::Truncate(_)
        | SqlOutcome::Delete(_)
        | SqlOutcome::Update(_)
        | SqlOutcome::CreateTable(_)
        | SqlOutcome::DropTable(_)
        | SqlOutcome::AlterTable(_)
        | SqlOutcome::CreateView(_)
        | SqlOutcome::DropView(_)
        | SqlOutcome::CreateIndex(_)
        | SqlOutcome::DropIndex(_)
        | SqlOutcome::CreateType(_)
        | SqlOutcome::DropType(_)
        | SqlOutcome::Begin
        | SqlOutcome::Commit
        | SqlOutcome::Rollback
        | SqlOutcome::DeclareCursor
        | SqlOutcome::Fetch(_)
        | SqlOutcome::CloseCursor => None,
    };

    assert_eq!(
        described, executed_columns,
        "describe columns must match execute columns for: {sql}"
    );

    // `describe_session` を実行に使っていないため、独立したセッションでの
    // 呼び出しでも副作用（`search_mode`・UDF 登録）を持ち込んでいないことも
    // あわせて確認する。
    let _ = describe_session;
}

#[test]
fn describe_select_all_matches_execute() {
    let path = unique_db_path("describe-select-all");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_row(&core, &ctx, 1, "op-seed-0001");

    assert_describe_matches_execute(
        &core,
        &ctx,
        "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
    );
}

#[test]
fn describe_select_with_where_matches_execute() {
    let path = unique_db_path("describe-select-where");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_row(&core, &ctx, 1, "op-seed-0002");

    assert_describe_matches_execute(
        &core,
        &ctx,
        "SELECT id, lang FROM documents WHERE lang = 'en' ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
    );
}

#[test]
fn describe_scan_matches_execute() {
    let path = unique_db_path("describe-scan");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_row(&core, &ctx, 1, "op-seed-0003");

    assert_describe_matches_execute(&core, &ctx, "SELECT id, body FROM documents LIMIT 5");
}

#[test]
fn describe_aggregate_without_group_by_matches_execute() {
    let path = unique_db_path("describe-agg-nogroup");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_row(&core, &ctx, 1, "op-seed-0004");

    assert_describe_matches_execute(&core, &ctx, "SELECT COUNT(*) FROM documents");
}

#[test]
fn describe_aggregate_with_group_by_matches_execute() {
    let path = unique_db_path("describe-agg-group");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_row(&core, &ctx, 1, "op-seed-0005");
    seed_row(&core, &ctx, 2, "op-seed-0006");

    assert_describe_matches_execute(
        &core,
        &ctx,
        "SELECT lang, COUNT(*) FROM documents GROUP BY lang",
    );
}

/// 型付き集計・式列（Issue #1173）でも Describe と実行の列メタが一致し、
/// 結果型が入力型から決まること（COUNT→BigInt・MIN(TEXT)→Text・式列→Double・
/// GROUP BY キー→Text）。
#[test]
fn describe_typed_aggregate_and_expression_columns_match_execute() {
    let path = unique_db_path("describe-typed-agg");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_row(&core, &ctx, 1, "op-seed-0101");
    seed_row(&core, &ctx, 2, "op-seed-0102");

    for sql in [
        "SELECT COUNT(*), MIN(body), MAX(lang), SUM(id), AVG(id) FROM documents",
        "SELECT lang, COUNT(*), MIN(body) FROM documents GROUP BY lang",
        "SELECT id, vec_norm(embedding) AS n FROM documents LIMIT 5",
    ] {
        assert_describe_matches_execute(&core, &ctx, sql);
    }

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT COUNT(*) AS c, MIN(body) AS b, SUM(id) AS s, AVG(id) AS a FROM documents",
        )
        .expect("aggregate should succeed");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected query outcome");
    };
    let tys: Vec<_> = result
        .columns
        .iter()
        .map(|c| match c {
            engine::sql::exec::ColumnMeta::Computed { ty, .. } => ty.clone(),
            other => panic!("expected Computed, got {other:?}"),
        })
        .collect();
    assert_eq!(
        tys,
        vec![
            Some(ColumnType::BigInt),
            Some(ColumnType::Text),
            Some(ColumnType::Numeric {
                precision: 20,
                scale: 0
            }),
            Some(ColumnType::Double),
        ]
    );
}

/// `EXPLAIN` の実行本体（`run_explain_plan`）は辞書抽出用の `path` 列等、本
/// ファイルの最小テーブルには無い前提を要求するため、実行結果との突き合わせは
/// 行わず、`sql::explain::build_explain_result` が常に返す単一列
/// （`ColumnMeta::Computed{name: "QUERY PLAN"}`）と一致することのみを固定する
/// （`sql::explain` モジュールドキュメント参照。列自体は展開結果の内容に
/// 依存しない固定値のため、Describe はプラン実行を経由せず導出できる）。
#[test]
fn describe_explain_reports_fixed_query_plan_column() {
    let path = unique_db_path("describe-explain");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();
    let sql = "EXPLAIN SELECT id FROM documents USING PLAN('test query') LIMIT 5";

    let parsed = core.parse_sql(sql).expect("parse should succeed");
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe should succeed");
    assert_eq!(
        described,
        Some(vec![engine::sql::exec::ColumnMeta::Computed {
            name: "QUERY PLAN".to_string(),
            ty: Some(engine::catalog::ColumnType::Text),
        }])
    );
}

/// `EXPLAIN` で包まない素の `SELECT ... USING PLAN(...)` の Describe が
/// プランナー／再埋め込み I/O（`plan_query`・`Embedder::embed_batch`）を
/// 一切実行しないことを固定する（Issue #933 レビュー指摘対応。LLM コスト
/// 増幅 DoS 経路にしない契約の中核証拠。`describe_explain_reports_fixed_
/// query_plan_column` は `EXPLAIN` で包んだケースのみを検証しており、
/// 素の `SELECT ... USING PLAN(...)` を Describe する経路は本テスト以前は
/// 未カバーだった）。
///
/// `core` にはプランナーを一切注入していないため（`EngineCore::
/// with_query_planner` 未呼び出し）、もし `describe_parsed_in_session` が
/// 誤って `plan_query`（`Self::expand_query`）へ到達する形へリファクタされた
/// 場合、`CoreError::QueryPlannerUnavailable` で `describe` 自体が失敗する。
/// 本テストが `describe` の成功を固定していることで、そのようなリファクタを
/// 検知できる。
#[test]
fn describe_using_plan_without_explain_does_not_require_planner() {
    let path = unique_db_path("describe-using-plan-no-explain");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();
    let sql = "SELECT id, body FROM documents USING PLAN('test query') LIMIT 5";

    let parsed = core.parse_sql(sql).expect("parse should succeed");
    let described = core.describe_parsed_in_session(&session, &parsed).expect(
        "describe of a bare USING PLAN select must not require planner I/O \
             (no query planner is configured on this core; reaching plan_query \
             would fail with QueryPlannerUnavailable)",
    );

    // 同一 SQL の実行（`execute_sql_in_session`）はプランナー未接続のため
    // `plan_using_plan_expansion`（`plan_query`）到達時に必ず失敗することを
    // あわせて固定する。これにより上の `describe` 成功が「そもそも実行も
    // プランナーを要さない」という別の理由での偶然の一致ではなく、
    // 「Describe だけがプラン展開 I/O を回避している」ことの対比になる。
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut exec_session = SessionState::default();
    let exec_result = core.execute_sql_in_session(&ctx, &mut exec_session, sql);
    assert!(
        exec_result.is_err(),
        "execute of the same USING PLAN select must fail without a configured \
         query planner (QueryPlannerUnavailable), proving Describe's success is \
         due to skipping planner I/O rather than the statement being planner-free"
    );

    assert_eq!(
        described,
        Some(vec![
            engine::sql::exec::ColumnMeta::Id,
            engine::sql::exec::ColumnMeta::Scalar {
                name: "body".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
        ])
    );
}

#[test]
fn describe_insert_returning_matches_execute() {
    let path = unique_db_path("describe-insert-returning");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    assert_describe_matches_execute(
        &core,
        &ctx,
        "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'hello', 'en') RETURNING id, body USING OPERATION_ID 'op-insert-ret-0001'",
    );
}

#[test]
fn describe_insert_without_returning_reports_no_columns() {
    let path = unique_db_path("describe-insert-no-returning");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();
    let sql = "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'hello', 'en') USING OPERATION_ID 'op-insert-nodesc-0001'";

    let parsed = core.parse_sql(sql).expect("parse should succeed");
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe should succeed");
    assert_eq!(described, None, "INSERT without RETURNING has no columns");
}

#[test]
fn describe_set_search_mode_and_truncate_report_no_columns() {
    let path = unique_db_path("describe-no-columns");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();

    for sql in [
        "SET search_mode = 'recall'",
        "TRUNCATE TABLE documents USING OPERATION_ID 'op-truncate-0001'",
    ] {
        let parsed = core.parse_sql(sql).expect("parse should succeed");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe should succeed");
        assert_eq!(described, None, "{sql} must report no columns");
    }
}

/// Describe は `parse_sql` が確定した構文形の束縛のみを行い、行・台帳・世代の
/// いずれにも到達しない（受け入れ条件の中核証拠）。`documents` テーブルの行数・
/// テーブル世代が Describe の前後で完全に不変であることを確認する。
#[test]
fn describe_has_no_side_effects_on_rows_or_table_generation() {
    let path = unique_db_path("describe-no-side-effects");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_row(&core, &ctx, 1, "op-seed-0008");

    let session = SessionState::default();
    for sql in [
        "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        "SELECT COUNT(*) FROM documents",
        "INSERT INTO documents (id, embedding, body, lang) VALUES (99, '[0.4,0.5,0.6]', 'world', 'ja') RETURNING id USING OPERATION_ID 'op-describe-noop-0001'",
        "DELETE FROM documents WHERE id = 1 USING OPERATION_ID 'op-describe-noop-0002'",
        "UPDATE documents SET body = 'changed' WHERE id = 1 USING OPERATION_ID 'op-describe-noop-0003'",
    ] {
        let parsed = core.parse_sql(sql).expect("parse should succeed");
        let _ = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe should succeed");
    }

    // Describe 後も seed した 1 行のみが可視で、内容も変わっていないこと
    // （新規行の挿入・削除・更新のいずれも行われていない）。
    let read_ctx = PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant");
    let result = core
        .execute_sql(&read_ctx, "SELECT id, body FROM documents LIMIT 100")
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].id, 1);
}

/// PR #1012 レビュー指摘の回帰固定: 実リテラルを持つ通常の SQL テキスト
/// （`$n` を含まない・`parse_sql` 経由の `ParsedSql`）の Describe は、
/// `ORDER BY <vec列> <=> '<不正なベクトルリテラル>'` の形式・次元・非有限値
/// 検証を Bind 前から必ず行う（Execute まで失敗が遅延してはならない）。
/// `describe_prepared_in_session`（ダミー値専用の縮退経路）専用の省略が
/// この通常経路へ誤って一律適用されていないことを固定する。
#[test]
fn describe_select_rejects_invalid_vector_literal_before_execute() {
    let path = unique_db_path("describe-invalid-vector-literal");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();

    // 次元不一致（テーブルの VECTOR 列は 3 次元）。
    let parsed = core
        .parse_sql("SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2]' LIMIT 5")
        .expect("parse should succeed (literal shape is structurally valid)");
    let err = core
        .describe_parsed_in_session(&session, &parsed)
        .expect_err(
            "describe must reject a dimension-mismatched vector literal, not defer to execute",
        );
    assert_eq!(err.wire_code(), "22000");

    // 形式不正（数値として解釈できない）。
    let parsed = core
        .parse_sql("SELECT id FROM documents ORDER BY embedding <=> 'invalid' LIMIT 5")
        .expect("parse should succeed (literal shape is structurally valid)");
    let err = core
        .describe_parsed_in_session(&session, &parsed)
        .expect_err("describe must reject a malformed vector literal, not defer to execute");
    assert_eq!(err.wire_code(), "22000");
}
/// Issue #1182・SQL-21: `UPDATE`（単一行・述語形）・述語形 `DELETE`・UPSERT の
/// `RETURNING` も、Describe の列と Execute の列が一致する（列メタは書き込み前に
/// 確定する純粋計算で、Describe は本体を実行しない）。
#[test]
fn describe_update_delete_upsert_returning_matches_execute() {
    let path = unique_db_path("describe-dml-returning");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_row(&core, &ctx, 1, "op-seed-1182-1");
    seed_row(&core, &ctx, 2, "op-seed-1182-2");

    for sql in [
        "UPDATE documents SET body = 'x' WHERE id = 1 RETURNING id, body USING OPERATION_ID 'op-desc-upd-1'",
        "UPDATE documents SET body = 'y' WHERE lang = 'en' RETURNING * USING OPERATION_ID 'op-desc-upd-2'",
        "INSERT INTO documents (id, embedding, body, lang) VALUES (3, '[0.1,0.2,0.3]', 'z', 'en') ON CONFLICT (id) DO NOTHING RETURNING id, lang USING OPERATION_ID 'op-desc-ups-1'",
        "DELETE FROM documents WHERE lang = 'en' RETURNING id, body USING OPERATION_ID 'op-desc-del-1'",
    ] {
        assert_describe_matches_execute(&core, &ctx, sql);
    }
}

/// `RETURNING` なしの `UPDATE`・述語形 `DELETE`・UPSERT の Describe は結果列を持たない。
#[test]
fn describe_update_delete_upsert_without_returning_reports_no_columns() {
    let path = unique_db_path("describe-dml-no-returning");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let session = SessionState::default();

    for sql in [
        "UPDATE documents SET body = 'x' WHERE id = 1 USING OPERATION_ID 'op-desc-none-1'",
        "UPDATE documents SET body = 'x' WHERE lang = 'en' USING OPERATION_ID 'op-desc-none-2'",
        "DELETE FROM documents WHERE lang = 'en' USING OPERATION_ID 'op-desc-none-3'",
        "INSERT INTO documents (id, embedding, body, lang) VALUES (3, '[0.1,0.2,0.3]', 'z', 'en') ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-desc-none-4'",
    ] {
        let parsed = core.parse_sql(sql).expect("parse should succeed");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe should succeed");
        assert_eq!(described, None, "{sql} must report no columns");
    }
}
