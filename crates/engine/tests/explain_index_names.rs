//! `EXPLAIN` の `scalar_plan:`／`ann_plan:` 行への使用索引名注記（Issue #1066・
//! TASK-206・INDEX-7・SQL-6・SQL-27）の結合テスト。
//!
//! `tests/index_declaration_targets.rs`（Issue #1065。宣言が構築対象へ効く
//! ことの固定）・`tests/sql_explain.rs`（既存 `EXPLAIN` 行の固定）と役割を
//! 分け、本ファイルは「索引経路を使う場合に索引名が付き、使わない場合は
//! 既存出力とビット同一のまま」という Issue #1066 の受け入れ条件を固定する。
//! `docs/design/explain-search-engine-exposure.md`「追記（Issue #1066）」参照。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine::{self, HnswScope};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;
use engine::storage::Visibility;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 4;
// `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超。HNSW opt-in の `ann_plan:`
// を `hnsw_full_visible`／`hnsw_subset` にするために必要（`tests/
// index_declaration_targets.rs` と同じ既定値）。
const HNSW_ROWS: usize = 1_100;

fn schema(table: &str) -> TableSchema {
    TableSchema::new(
        table,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("topic", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public]).expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn seed_rows(storage: &Storage, table: &str, tenant: &str, rows: usize) {
    let ctx = ctx(tenant);
    for i in 0..rows {
        let kind = if i % 2 == 0 { "a" } else { "b" };
        let topic = if i % 3 == 0 { "x" } else { "y" };
        let op_id =
            OperationId::parse(&format!("seed-{table}-{tenant}-{i}")).expect("valid operation id");
        engine::tenant::insert_typed_row(
            storage,
            table,
            &ctx,
            i as u64,
            Visibility::Public,
            &[
                Value::Vector(vec![i as f32, 0.0, 0.0, 0.0]),
                Value::Text(kind.to_string()),
                Value::Text(topic.to_string()),
            ],
            &op_id,
        )
        .expect("insert row");
    }
}

/// opt-in の `EngineCore`（HNSW）を 1 テーブル・`HNSW_ROWS` 行で構築する。
fn hnsw_opt_in_core(label: &str, scope: HnswScope) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema("docs")).expect("create table");
    seed_rows(&storage, "docs", "tenant-a", HNSW_ROWS);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(scope);
    (core, guard)
}

/// 既定エンジン（opt-in なし）の `EngineCore` を構築する。
fn default_core(label: &str, rows: usize) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema("docs")).expect("create table");
    seed_rows(&storage, "docs", "tenant-a", rows);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, guard)
}

/// `sql` の `EXPLAIN` 応答（テナント `tenant`）を `Vec<String>` へ揃える。
fn explain_lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("EXPLAIN failed for {sql:?}: {e:?}"));
    match outcome {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

fn find_line<'a>(lines: &'a [String], prefix: &str) -> &'a str {
    lines
        .iter()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("missing {prefix} line in {lines:?}"))
        .as_str()
}

/// 受け入れ条件 3（後方互換）: HNSW opt-in・宣言なしでは、宣言前後で
/// `EXPLAIN` の全行がビット同一（自動索引には名前が付かない）。
#[test]
fn no_declarations_produces_no_index_name_suffix() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-no-decl", HnswScope::All);
    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert!(
        !find_line(&lines, "ann_plan: ").contains("index="),
        "{lines:?}"
    );
    assert!(
        !find_line(&lines, "scalar_plan: ").contains("index="),
        "{lines:?}"
    );
}

/// スカラー単一列: 宣言列 1 つへの等価述語は `index_equality index=<name>`。
#[test]
fn scalar_single_column_equality_reports_declared_index_name() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-scalar-single", HnswScope::All);
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind ON docs (kind)",
    )
    .expect("declare scalar index");

    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert_eq!(
        find_line(&lines, "scalar_plan: "),
        "scalar_plan: index_equality index=idx_kind"
    );
}

/// 交差・ソート: 2 列を別々の宣言で被覆すると両索引名が昇順で出る。
#[test]
fn scalar_conjunction_reports_sorted_deduplicated_index_names() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-scalar-conj", HnswScope::All);
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind ON docs (kind)",
    )
    .expect("declare idx_kind");
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_topic ON docs (topic)",
    )
    .expect("declare idx_topic");

    let sql = "SELECT id FROM docs WHERE kind = 'a' AND topic = 'x' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    // 索引名は昇順（idx_kind < idx_topic）。
    assert_eq!(
        find_line(&lines, "scalar_plan: "),
        "scalar_plan: index_conjunction index=idx_kind,idx_topic"
    );
}

/// 非被覆列: `kind` だけ宣言し、宣言外の `topic` を絞ると索引名は出ない
/// （実行時は `FallbackNoIndex` で全走査に落ちるため索引使用を主張しない）。
#[test]
fn uncovered_column_reports_no_index_name() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-uncovered", HnswScope::All);
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind ON docs (kind)",
    )
    .expect("declare idx_kind only");

    let sql = "SELECT id FROM docs WHERE topic = 'x' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert!(
        !find_line(&lines, "scalar_plan: ").contains("index="),
        "{lines:?}"
    );
}

/// `id` 述語のみ（`index_id_range`）は名前なし。`id` と宣言列の交差は
/// 宣言列の索引名だけを出す（`id` は常設索引で宣言索引ではない）。
#[test]
fn id_predicate_is_excluded_from_index_name_reporting() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-id-pred", HnswScope::All);
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind ON docs (kind)",
    )
    .expect("declare idx_kind");

    let id_only_sql = "SELECT id FROM docs WHERE id > 5 ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let id_only_lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {id_only_sql}"));
    assert_eq!(
        find_line(&id_only_lines, "scalar_plan: "),
        "scalar_plan: index_id_range"
    );

    let mixed_sql =
        "SELECT id FROM docs WHERE id > 5 AND kind = 'a' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let mixed_lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {mixed_sql}"));
    assert_eq!(
        find_line(&mixed_lines, "scalar_plan: "),
        "scalar_plan: index_conjunction index=idx_kind"
    );
}

/// HNSW `--hnsw-scope all`: `HnswScope::All` は `hnsw_targeted_in_txn` が
/// 宣言の有無を見ず常に適格（`true`）とするため、宣言の追加・削除で実行経路
/// は変わらない。索引名注記はその「実際に使われた索引」を示す契約と矛盾する
/// ため、`All` では宣言の有無によらず `ann_plan:` に索引名を付けない
/// （codex-review P1 指摘対応・Issue #1066 PR #1155）。
#[test]
fn hnsw_scope_all_never_reports_index_name() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-hnsw-all", HnswScope::All);
    let mut session = allowed_session();

    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let before = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert_eq!(
        find_line(&before, "ann_plan: "),
        "ann_plan: hnsw_full_visible"
    );

    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_vec ON docs USING hnsw (embedding)",
    )
    .expect("declare hnsw index");
    let declared = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert_eq!(
        declared, before,
        "declaring an index under HnswScope::All must not change EXPLAIN output"
    );

    // フィルタ付き（`hnsw_subset`）でも同様に名前は付かない。
    let filtered_sql =
        "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let filtered = explain_lines(&core, "tenant-a", &format!("EXPLAIN {filtered_sql}"));
    assert_eq!(find_line(&filtered, "ann_plan: "), "ann_plan: hnsw_subset");

    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, "DROP INDEX idx_vec")
        .expect("drop hnsw declaration");
    let dropped = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert_eq!(dropped, before, "drop index must restore the prior output");
}

/// HNSW `--hnsw-scope declared`: この scope では対象テーブルの HNSW 適格性
/// そのものが宣言の有無で決まる（`catalog::hnsw_targeted_in_txn`）ため、
/// 宣言名を「使用索引名」として注記してよい。宣言前は厳密（brute-force）の
/// まま `index=` は付かず、宣言後に付き、`DROP INDEX` で再び消える。
#[test]
fn hnsw_scope_declared_reports_and_clears_index_name() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-hnsw-declared", HnswScope::Declared);
    let mut session = allowed_session();

    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let before = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert!(
        !find_line(&before, "ann_plan: ").contains("index="),
        "{before:?}"
    );

    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_vec ON docs USING hnsw (embedding)",
    )
    .expect("declare hnsw index");
    let declared = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert_eq!(
        find_line(&declared, "ann_plan: "),
        "ann_plan: hnsw_full_visible index=idx_vec"
    );

    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, "DROP INDEX idx_vec")
        .expect("drop hnsw declaration");
    let dropped = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert_eq!(dropped, before, "drop index must restore the prior output");
}

/// codex-review P1 指摘対応（Issue #1066 PR #1155）: 述語列を複数の宣言が
/// 重複して覆う場合、実行側 `declared_index_targets_in_txn` は宣言名を捨て
/// 列の和集合から単一の `ScalarIndex` を構築するため、個別に使われる経路が
/// 無い宣言まで `index=` に表示してはならない。最小の被覆集合（貪欲法。
/// 被覆数が同数なら名前の昇順）へ縮退する。
#[test]
fn scalar_overlapping_declarations_report_minimal_covering_index_name() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-scalar-overlap", HnswScope::All);
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind_only ON docs (kind)",
    )
    .expect("declare idx_kind_only");
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind_topic ON docs (kind, topic)",
    )
    .expect("declare idx_kind_topic");

    // `kind` だけの述語は両宣言が被覆するが、実行側が使う経路は 1 つの
    // 合成索引であり、両方を「使用索引」として表示すると個別使用を偽装する。
    // 貪欲法は被覆数が同数（ともに `kind` の 1 列）のとき名前の昇順で選ぶため
    // `idx_kind_only` だけを表示する。
    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert_eq!(
        find_line(&lines, "scalar_plan: "),
        "scalar_plan: index_equality index=idx_kind_only"
    );
}

/// 集計 EXPLAIN: `WHERE` が宣言列のみで構成される集計は
/// `access_path: scalar_index_candidates` かつ索引名が付く。`GROUP BY`
/// （`WHERE` なし）は名前なし（`access_path` が異なる形のため）。
#[test]
fn aggregate_explain_reports_index_name_only_for_scalar_index_candidates() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-aggregate", HnswScope::All);
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind ON docs (kind)",
    )
    .expect("declare idx_kind");

    let count_sql = "SELECT COUNT(*) FROM docs WHERE kind = 'a'";
    let count_lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {count_sql}"));
    assert_eq!(
        find_line(&count_lines, "scalar_plan: "),
        "scalar_plan: index_equality index=idx_kind"
    );
    assert_eq!(
        find_line(&count_lines, "access_path: "),
        "access_path: scalar_index_candidates"
    );

    let group_by_sql = "SELECT kind, COUNT(*) FROM docs GROUP BY kind";
    let group_by_lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {group_by_sql}"));
    assert!(
        !find_line(&group_by_lines, "scalar_plan: ").contains("index="),
        "{group_by_lines:?}"
    );
}

/// テナント非漏えい: 可視行数が異なる 2 テナントで `EXPLAIN` の全行が同一
/// （行数・カーディナリティ等のテナント存在情報に繋がる値を含まない）。
#[test]
fn explain_output_is_identical_across_tenants_with_different_visible_row_counts() {
    let (core, _guard) = hnsw_opt_in_core("explain-idx-names-tenant-parity", HnswScope::All);
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind ON docs (kind)",
    )
    .expect("declare idx_kind");
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_vec ON docs USING hnsw (embedding)",
    )
    .expect("declare hnsw index");

    // tenant-b は可視行 0 件（`docs` には tenant-a の行しか無い）。
    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let a_lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    let b_lines = explain_lines(&core, "tenant-b", &format!("EXPLAIN {sql}"));
    assert_eq!(
        a_lines, b_lines,
        "EXPLAIN output (including index name suffixes) must not depend on visible row count"
    );
}

/// fail-closed: opt-in なしでは（`declarations_enabled` が偽のため）宣言が
/// あっても `scalar_plan:` に索引名は付かない（Issue #1065 の既存契約と
/// 同じ「宣言は opt-in なしでは無効」を索引名注記にも適用する）。
#[test]
fn declarations_without_hnsw_opt_in_report_no_index_names() {
    let (core, _guard) = default_core("explain-idx-names-no-optin", 50);
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_kind ON docs (kind)",
    )
    .expect("declare idx_kind");

    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 5";
    let lines = explain_lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert!(
        !find_line(&lines, "scalar_plan: ").contains("index="),
        "{lines:?}"
    );
}
