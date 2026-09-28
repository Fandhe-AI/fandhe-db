//! `EXPLAIN` の `scalar_plan:`／`access_path:` 表示が、索引宣言（`CREATE
//! INDEX`）による構築対象選択と一致することを固定する結合テスト（Issue
//! #1153・TASK-206・INDEX-7・SQL-27・NOSQL-16）。
//!
//! `crates/engine/tests/index_declaration_targets.rs`（実行時の索引経路切替
//! 固定）・`crates/engine/tests/sql27_explain_targets.rs`（EXPLAIN 対象拡大の
//! 静的判定）の両フィクスチャ流儀を踏まえ、本ファイルは「宣言で対象外にした
//! 列への述語は EXPLAIN も plain scan（全走査）を報告する」こと——つまり
//! `EXPLAIN` の表示と実行時の索引消費（`plain_scan_fallbacks`／
//! `index_scans`）が一致すること——に焦点を当てる。opt-in なしでは表示が
//! 従来どおり変わらないことも合わせて固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 4;
// `sql::scalar_index::CandidateResolution::FallbackSelectivity`（選択度に
// よる縮退。本 Issue のスコープ外——モジュールドキュメント参照）を誤って
// 誘発しないよう、`index_declaration_targets.rs` と同じ規模の行数を使う
// （行数が少なすぎると宣言列への述語でも選択度縮退が働き、本テストが検証
// したい「宣言による対象外化」以外の理由で plain scan へ落ちてしまう）。
const ROWS: usize = 1_100;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn schema(table: &str) -> TableSchema {
    TableSchema::new(
        table,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("topic", ColumnType::Text, false),
        ],
    )
}

fn insert_row(storage: &Storage, table: &str, tenant: &str, id: u64, lang: &str, topic: &str) {
    let op_id =
        OperationId::parse(&format!("explain-decl-op-{table}-{id}")).expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(tenant),
        id,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0, 0.0, 0.0, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(topic.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

/// `lang`（`id % 3 == 0` → `ja`・それ以外 `en`）・`topic`（`id % 2 == 0` →
/// `alpha`・それ以外 `beta`）を持つ行を `ROWS` 件、`id in [1, ROWS]` へ一括
/// 挿入する（`index_declaration_targets.rs::seed_rows` と同じ分布方針。
/// 選択度縮退を避けるだけの規模で埋め、各列の値ごとの行数を十分確保する）。
fn seed_rows(storage: &Storage, table: &str, tenant: &str) {
    let ctx = ctx(tenant);
    let schema = schema(table);
    let metadata_bufs: Vec<Vec<u8>> = (1..=ROWS)
        .map(|id| {
            let lang = if id % 3 == 0 { "ja" } else { "en" };
            let topic = if id % 2 == 0 { "alpha" } else { "beta" };
            engine::row_codec::encode_scalar_columns(
                &schema,
                &[
                    Value::Null,
                    Value::Text(lang.to_string()),
                    Value::Text(topic.to_string()),
                ],
            )
            .expect("encode metadata")
        })
        .collect();
    let rows: Vec<(u64, RowInput<'_>)> = (1..=ROWS)
        .map(|id| {
            (
                id as u64,
                RowInput {
                    tenant_id: tenant,
                    visibility: Visibility::Public,
                    embedding: &[0.0, 0.0, 0.0, 0.0],
                    metadata: metadata_bufs[id - 1].as_slice(),
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(&format!("explain-decl-seed-{table}")).expect("valid op_id");
    engine::tenant::insert_rows(storage, table, &ctx, &rows, &op_id).expect("seed rows batch");
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

/// `EXPLAIN` の行（`QUERY PLAN` 単一列）をテキスト列へ変換する
/// （`sql27_explain_targets.rs::explain_lines` と同構成）。
fn explain_lines(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Explain(result) => {
            assert_eq!(result.columns.len(), 1);
            assert_eq!(
                result.columns[0],
                ColumnMeta::Computed {
                    name: "QUERY PLAN".to_string()
                }
            );
            result
                .rows
                .iter()
                .map(|row| match &row.cells[0] {
                    Cell::Text(s) => s.clone(),
                    other => panic!("expected Cell::Text, got {other:?}"),
                })
                .collect()
        }
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

fn explain_sql(core: &EngineCore, session: &mut SessionState, sql: &str) -> Vec<String> {
    let outcome = core
        .execute_sql_in_session(&ctx("tenant-a"), session, sql)
        .unwrap_or_else(|e| panic!("EXPLAIN {sql:?} failed: {e:?}"));
    explain_lines(outcome)
}

fn scalar_plan_line(lines: &[String]) -> &str {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("scalar_plan: "))
        .unwrap_or_else(|| panic!("no scalar_plan: line in {lines:?}"))
}

fn access_path_line(lines: &[String]) -> &str {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("access_path: "))
        .unwrap_or_else(|| panic!("no access_path: line in {lines:?}"))
}

/// HNSW opt-in（起動時スイッチ）済みの `EngineCore` と、`lang`／`topic`
/// （いずれも `TEXT`）の行を `ROWS` 件持つ `docs` テーブルを用意する。
/// 返す [`CleanupGuard`] は呼び出し元が `EngineCore` と同じスコープで
/// 保持すること（先に宣言した `Storage` より後に drop させ、ファイル
/// ハンドルを閉じてから削除する `temp_db` モジュールの契約に従う）。
fn setup_hnsw_core() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("explain-scalar-plan-decl");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema("docs")).expect("create table");
    seed_rows(&storage, "docs", "tenant-a");

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    (EngineCore::from_storage_with_engine(storage, kind), guard)
}

/// 受け入れ条件 1・3: 宣言後、宣言列（`lang`）への述語は `scalar_plan:` が
/// 索引経路を報告し、宣言外の列（`topic`）は `plain_scan` へ降格する。
/// `id` 述語（`id_index` は宣言の有無によらず常に構築される）は降格しない。
#[test]
fn search_explain_reflects_scalar_declaration_target() {
    let (core, _guard) = setup_hnsw_core();
    let mut session = allowed_session();

    // 宣言前: `topic` 述語も索引経路（現行の自動挙動）。
    let before = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT id FROM docs WHERE topic = 'alpha' \
         ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5",
    );
    assert_eq!(scalar_plan_line(&before), "index_equality");

    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_lang ON docs (lang)",
    )
    .expect("declare scalar index on lang only");

    let lang_eq = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT id FROM docs WHERE lang = 'ja' \
         ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5",
    );
    assert_eq!(
        scalar_plan_line(&lang_eq),
        "index_equality",
        "declared column lang keeps its index-eligible token"
    );

    let topic_eq = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT id FROM docs WHERE topic = 'alpha' \
         ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5",
    );
    assert_eq!(
        scalar_plan_line(&topic_eq),
        "plain_scan",
        "undeclared column topic downgrades to plain_scan once declarations are active"
    );

    let conjunction = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT id FROM docs WHERE lang = 'ja' AND topic = 'alpha' \
         ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5",
    );
    assert_eq!(
        scalar_plan_line(&conjunction),
        "plain_scan",
        "a predicate on any undeclared column downgrades the whole conjunction"
    );

    let id_range = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT id FROM docs WHERE id > 1 \
         ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5",
    );
    assert_eq!(
        scalar_plan_line(&id_range),
        "index_id_range",
        "id predicates are not gated by scalar column declarations (id_index is always built)"
    );

    let lang_and_id = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT id FROM docs WHERE lang = 'ja' AND id > 1 \
         ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5",
    );
    assert_eq!(
        scalar_plan_line(&lang_and_id),
        "index_conjunction",
        "declared column + id predicate stays index-eligible"
    );

    // `DROP INDEX` の後: `topic` の表示が宣言前の索引経路へ戻る。
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, "DROP INDEX idx_lang")
        .expect("drop scalar index declaration");
    let after_drop = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT id FROM docs WHERE topic = 'alpha' \
         ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5",
    );
    assert_eq!(scalar_plan_line(&after_drop), "index_equality");
}

/// `EXPLAIN` の表示と実行時の索引消費統計（`plain_scan_fallbacks`／
/// `index_scans`）が一致することを固定する（`sql::exec` の実経路と
/// `sql::explain` の静的表示が同じ単一情報源から導出されることの検証）。
#[test]
fn search_explain_matches_runtime_scalar_index_stats() {
    let (core, _guard) = setup_hnsw_core();
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_lang ON docs (lang)",
    )
    .expect("declare scalar index on lang only");

    let lang_query =
        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let topic_query =
        "SELECT id FROM docs WHERE topic = 'alpha' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";

    // `EXPLAIN` は行走査・キャッシュ消費を一切行わないため、統計観測は
    // 通常の `execute_sql` で行う（`index_declaration_targets.rs` と同じ
    // 「2 回目で観測」流儀。1 回目はキャッシュ未構築のため）。
    core.execute_sql(&ctx("tenant-a"), lang_query)
        .expect("lang query warm");
    let before_scans = core.scalar_index_cache_stats().index_scans;
    core.execute_sql(&ctx("tenant-a"), lang_query)
        .expect("lang query observed");
    let after_lang = core.scalar_index_cache_stats();
    assert!(
        after_lang.index_scans > before_scans,
        "declared column lang must consume the index at runtime: {after_lang:?}"
    );
    assert_eq!(
        scalar_plan_line(&explain_sql(
            &core,
            &mut session,
            &format!("EXPLAIN {lang_query}")
        )),
        "index_equality",
        "EXPLAIN must agree with the runtime index-scan path for lang"
    );

    core.execute_sql(&ctx("tenant-a"), topic_query)
        .expect("topic query warm");
    let before_fallbacks = core.scalar_index_cache_stats().plain_scan_fallbacks;
    core.execute_sql(&ctx("tenant-a"), topic_query)
        .expect("topic query observed");
    let after_topic = core.scalar_index_cache_stats();
    assert!(
        after_topic.plain_scan_fallbacks > before_fallbacks,
        "undeclared column topic must fall back to plain scan at runtime: {after_topic:?}"
    );
    assert_eq!(
        scalar_plan_line(&explain_sql(
            &core,
            &mut session,
            &format!("EXPLAIN {topic_query}")
        )),
        "plain_scan",
        "EXPLAIN must agree with the runtime plain-scan fallback for topic"
    );
}

/// 受け入れ条件 2: HNSW opt-in なし（既定エンジン）では、宣言の有無に関わらず
/// `scalar_plan:` の表示は従来どおり（ビット単位で不変）。
#[test]
fn search_explain_is_unchanged_without_hnsw_opt_in() {
    let path = unique_db_path("explain-scalar-plan-decl-no-optin");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema("docs")).expect("create table");
    insert_row(&storage, "docs", "tenant-a", 1, "ja", "alpha");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_lang ON docs (lang)",
    )
    .expect("declare scalar index (no-op without hnsw opt-in)");

    let topic_explain = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT id FROM docs WHERE topic = 'alpha' \
         ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5",
    );
    assert_eq!(
        scalar_plan_line(&topic_explain),
        "index_equality",
        "without hnsw opt-in, declarations must not change EXPLAIN output"
    );
}

/// 集計 `EXPLAIN`（`scalar_plan:`／`access_path:`）が索引宣言の対象選択を
/// 反映することを固定する（受け入れ条件 1）。
#[test]
fn aggregate_explain_reflects_scalar_declaration_target() {
    let (core, _guard) = setup_hnsw_core();
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_lang ON docs (lang)",
    )
    .expect("declare scalar index on lang only");

    let topic_count = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT COUNT(*) AS n FROM docs WHERE topic = 'alpha'",
    );
    assert_eq!(scalar_plan_line(&topic_count), "plain_scan");
    assert_eq!(access_path_line(&topic_count), "full_scan");

    let lang_count = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT COUNT(*) AS n FROM docs WHERE lang = 'ja'",
    );
    assert_eq!(scalar_plan_line(&lang_count), "index_equality");
    assert_eq!(access_path_line(&lang_count), "scalar_index_candidates");

    let group_by_topic = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT topic, COUNT(*) AS n FROM docs GROUP BY topic",
    );
    assert_eq!(
        access_path_line(&group_by_topic),
        "full_scan",
        "undeclared GROUP BY key cannot use the enumeration path"
    );

    let group_by_lang = explain_sql(
        &core,
        &mut session,
        "EXPLAIN SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang",
    );
    assert_eq!(
        access_path_line(&group_by_lang),
        "scalar_index_group_enumeration",
        "declared GROUP BY key keeps the enumeration path"
    );
}
