//! 索引宣言（`CREATE INDEX`／`DROP INDEX`）が `ScalarIndex`・`HnswIndexCache`
//! の構築対象へ実際に反映されることの結合テスト（Issue #1065・TASK-206・
//! INDEX-7。ポインタ: `docs/design/index-declaration-effects.md`）。
//!
//! `tests/sql_index_ddl.rs`（宣言の永続化・構文検証・既定エンジンでの結果
//! 不変性）・`tests/hnsw_cache.rs`（HNSW opt-in の Recall・キャッシュ結線）の
//! 両フィクスチャ流儀を組み合わせ、以下を固定する:
//!
//! - 起動時 opt-in（`SearchEngineKind::Hnsw`）なしでは、索引宣言の有無に
//!   関わらずスカラー索引・HNSW とも現行の自動挙動のまま変わらないこと
//!   （§2.1 上位スイッチ）
//! - opt-in ありでスカラー宣言（一部の列のみ）がある場合、宣言列への `WHERE`
//!   は索引経路（`index_scans`）を使い、宣言外の列への `WHERE` は plain scan
//!   （`plain_scan_fallbacks`）へ縮退すること
//! - opt-in ありで HNSW 宣言がカタログの一部テーブル（`table_a`）にのみある
//!   場合も、宣言のない他テーブル（`table_b`）の検索経路（HNSW）と Top-k 結果
//!   （SQL 表層・Rust API とも）は宣言の追加・削除の前後で一切変わらないこと
//!   （HNSW 適格性ゲートはテーブル単位。`docs/design/index-declaration-effects.md`
//!   「HNSW（テーブル単位）」）
//! - いずれの構成でも RLS 境界（テナント間非漏えい）は変わらないこと

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::mode::SessionState;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 8;
// `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超・`tests/hnsw_cache.rs` と
// 同じ既定値（本テストは Recall ではなく経路切り替えの固定が目的のため
// クラスタ構造は使わず単純な決定的擬似乱数で足りる）。
const HNSW_ROWS: usize = 1_100;

struct TestRng {
    state: u64,
}

impl TestRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_unit(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
    }
}

fn gen_vectors(seed: u64, dim: usize, rows: usize) -> Vec<Vec<f32>> {
    let mut rng = TestRng::new(seed);
    (0..rows)
        .map(|_| (0..dim).map(|_| rng.next_unit()).collect())
        .collect()
}

fn schema_with_vector(table: &str) -> TableSchema {
    TableSchema::new(
        table,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("topic", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed_rows(storage: &Storage, table: &str, tenant: &str, vectors: &[Vec<f32>], tag: &str) {
    let ctx = ctx(tenant);
    let schema = schema_with_vector(table);
    let metadata_bufs: Vec<Vec<u8>> = (0..vectors.len())
        .map(|i| {
            let lang = if i % 3 == 0 { "ja" } else { "en" };
            let topic = if i % 2 == 0 { "alpha" } else { "beta" };
            // 先頭要素は `embedding`（VECTOR 列）のプレースホルダ（`RowInput::
            // embedding` 側で別途保持するため常に `Value::Null`。
            // `tests/sql_aggregate.rs` と同じ流儀）。
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
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                i as u64,
                RowInput {
                    tenant_id: tenant,
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata: metadata_bufs[i].as_slice(),
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(&format!("seed-{table}-{tag}")).expect("valid operation id");
    engine::tenant::insert_rows(storage, table, &ctx, &rows, &op_id).expect("seed rows batch");
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

/// スカラー宣言（Issue #1065）が opt-in なしでは無効果（Auto のまま）である
/// ことを固定する。opt-in なしの既定エンジンで `CREATE INDEX` を宣言しても、
/// `WHERE lang = ...` は宣言前と同じ索引経路のまま（`index_scans` が増える）
/// で、`plain_scan_fallbacks` は増えない。
#[test]
fn scalar_declaration_has_no_effect_without_hnsw_opt_in() {
    let path = unique_db_path("index-decl-scalar-no-optin");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    let vectors = gen_vectors(1, DIM as usize, 50);
    seed_rows(&storage, "docs", "tenant-a", &vectors, "base");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_lang ON docs (lang)",
    )
    .expect("create scalar index declaration");

    let query =
        "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[0,0,0,0,0,0,0,0]' LIMIT 5";
    core.execute_sql(&ctx("tenant-a"), query)
        .expect("query with lang filter");
    // `ScalarIndexCache` は DISTANCE クエリ（`ORDER BY embedding <=> ...`）の
    // SCALAR 段でのみ消費される（`sql::scan::execute_scan` は `WHERE` のみの
    // クエリを別経路で処理しこのキャッシュを一切参照しない）。宣言（`lang`
    // のみ）の非適用を `plain_scan_fallbacks` で固定するには、宣言外の
    // `topic` 述語も DISTANCE クエリで観測しなければ検証にならない
    // （`tests/index_declaration_targets.rs` レビュー指摘: PR #1124）。
    core.execute_sql(
        &ctx("tenant-a"),
        "SELECT id FROM docs WHERE topic = 'alpha' ORDER BY embedding <=> '[0,0,0,0,0,0,0,0]' LIMIT 5",
    )
    .expect("query with topic filter");

    let stats = core.scalar_index_cache_stats();
    // opt-in なしでは宣言（`lang` のみ）の有無によらず全対応列（`lang`・
    // `topic` とも `TEXT`）が索引対象のまま——`topic` への述語も
    // `plain_scan_fallbacks` を増やさない。
    assert_eq!(
        stats.plain_scan_fallbacks, 0,
        "without hnsw opt-in, declared-only scoping must not apply: {stats:?}"
    );
}

/// opt-in ありでスカラー宣言（`lang` のみ）を作ると、宣言列 `lang` への
/// `WHERE` は索引経路を維持し、宣言外の `topic` への `WHERE` は plain scan
/// へ縮退することを固定する（§2.2「絞り込み」）。
#[test]
fn scalar_declaration_scopes_index_to_declared_columns_with_hnsw_opt_in() {
    let path = unique_db_path("index-decl-scalar-optin");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("docs"))
        .expect("create table");
    let vectors = gen_vectors(2, DIM as usize, HNSW_ROWS);
    seed_rows(&storage, "docs", "tenant-a", &vectors, "base");

    // テナント境界の非漏えい確認用の `Private` 行（`lang = 'ja'`。`Public` 行は
    // 本リポの設計上テナントを越えて可視〔`policy::PolicyContext::is_visible`〕
    // のため、`seed_rows` の `Public` 行では非漏えいの固定にならない）。
    let private_id = HNSW_ROWS as u64;
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx("tenant-a"),
        private_id,
        Visibility::Private,
        &[
            Value::Vector(vectors[0].clone()),
            Value::Text("ja".to_string()),
            Value::Text("alpha".to_string()),
        ],
        &OperationId::parse("seed-private").expect("valid operation id"),
    )
    .expect("insert private row");

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);
    let mut session = allowed_session();

    // `scalar_index::ScalarIndexCache` は DISTANCE クエリ（`sql::exec`）が
    // 消費する派生キャッシュのため、SCALAR 段の事前フィルタを伴う DISTANCE
    // クエリ（`WHERE ... ORDER BY embedding <=> ...`）で観測する（`WHERE` の
    // みで `ORDER BY` を伴わないクエリは `sql::scan::execute_scan` へ回り
    // このキャッシュを経由しない）。
    let q0 = &vectors[0];
    let dist = |col: &str, val: &str| {
        format!(
            "SELECT id FROM docs WHERE {col} = '{val}' ORDER BY embedding <=> '{}' LIMIT 5",
            vec_literal(q0)
        )
    };

    // 宣言前: `topic` 述語も索引経路を使う（現行の自動挙動）。
    core.execute_sql(&ctx("tenant-a"), &dist("topic", "alpha"))
        .expect("topic query before declaration");
    let before = core.scalar_index_cache_stats();
    assert_eq!(before.plain_scan_fallbacks, 0);

    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_lang ON docs (lang)",
    )
    .expect("declare scalar index on lang only");

    // 宣言後: `lang` は索引経路のまま。統計（`index_scans`）は
    // `ScalarIndexCache::lookup` がヒットした 2 回目以降の呼び出しでのみ
    // 計上される（1 回目は世代整合キャッシュが空で構築のみ）ため、同一世代内で
    // 2 回連続してクエリを投げる。
    let lang_query = dist("lang", "ja");
    core.execute_sql(&ctx("tenant-a"), &lang_query)
        .expect("lang query after declaration (warm)");
    let before_lang_scans = core.scalar_index_cache_stats().index_scans;
    core.execute_sql(&ctx("tenant-a"), &lang_query)
        .expect("lang query after declaration (observed)");
    let after_lang = core.scalar_index_cache_stats();
    assert!(
        after_lang.index_scans > before_lang_scans,
        "declared column lang must still use the index path: {after_lang:?}"
    );

    // 宣言後: 宣言外の `topic` は plain scan へ縮退する（同様に 2 回目で観測）。
    let topic_query = dist("topic", "alpha");
    core.execute_sql(&ctx("tenant-a"), &topic_query)
        .expect("topic query after declaration (warm)");
    let before_fallbacks = core.scalar_index_cache_stats().plain_scan_fallbacks;
    core.execute_sql(&ctx("tenant-a"), &topic_query)
        .expect("topic query after declaration (observed)");
    let after_topic = core.scalar_index_cache_stats();
    assert!(
        after_topic.plain_scan_fallbacks > before_fallbacks,
        "undeclared column topic must fall back to plain scan once declarations gate is active: {after_topic:?}"
    );

    // テナント境界: `Visibility::Private` 行は他テナントに一切見えない
    // （`Visibility::Public` は本リポの設計上テナントを越えて可視のため
    // （`policy::PolicyContext::is_visible`）、非漏えいの固定には `Private` 行を
    // 使う。`ctx("tenant-b")` は `tenant-a` の `Private` 行 `private_id` を
    // 一切返さないことを確認する）。
    let visible_ids: std::collections::HashSet<u64> = core
        .execute_sql(&ctx("tenant-b"), &lang_query)
        .expect("other tenant query")
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    assert!(
        !visible_ids.contains(&private_id),
        "tenant-a's private row must not leak into tenant-b's visible set: {visible_ids:?}"
    );
}

/// `HnswIndexCache` が照会された（＝HNSW 適格性ゲートを通過し HNSW 経路へ
/// 進んだ）回数の合計（新規構築 `builds` と索引済み探索 `hits`）。ゲート
/// 対象外（brute-force）ではどちらも増えない。
fn hnsw_path_count(core: &EngineCore) -> u64 {
    let stats = core.hnsw_index_cache_stats();
    stats.builds + stats.hits
}

/// Rust API（`VectorCore::search`）の Top-k を比較可能な形へ写す。
fn rust_api_top_k(core: &EngineCore, table: &str, query: &[f32]) -> Vec<(String, u64, u32)> {
    core.search(&ctx("tenant-a"), table, query, 5)
        .expect("rust api search")
        .into_iter()
        .map(|h| (h.tenant_id, h.id, h.score.to_bits()))
        .collect()
}

/// opt-in ありで `table_a` にだけ HNSW 宣言を追加・削除しても、宣言のない
/// `table_b` の検索経路（HNSW）と Top-k 結果（SQL 表層・Rust API とも）が
/// 一切変わらないことを固定する（Issue #1065 の「クエリ結果が宣言の有無で
/// 変わらない」受け入れ条件。旧カタログ全体単位ゲートでは `table_a` への
/// 宣言で `table_b` が近似〔HNSW〕から厳密〔brute-force〕へ切り替わり Top-k が
/// 変わり得た回帰の防止。`docs/design/index-declaration-effects.md`
/// 「HNSW（テーブル単位）」）。宣言した `table_a` 自身も宣言前と同じく HNSW
/// 経路のまま（HNSW 宣言は経路選択を変えない）。
#[test]
fn hnsw_declaration_on_one_table_does_not_change_other_table_results_or_path() {
    let path = unique_db_path("index-decl-hnsw-per-table");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("table_a"))
        .expect("create table_a");
    storage
        .create_table(&schema_with_vector("table_b"))
        .expect("create table_b");
    let vectors_a = gen_vectors(3, DIM as usize, HNSW_ROWS);
    let vectors_b = gen_vectors(4, DIM as usize, HNSW_ROWS);
    seed_rows(&storage, "table_a", "tenant-a", &vectors_a, "base");
    seed_rows(&storage, "table_b", "tenant-a", &vectors_b, "base");

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);
    let mut session = allowed_session();

    let q_a = format!(
        "SELECT id FROM table_a ORDER BY embedding <=> '{}' LIMIT 5",
        vec_literal(&vectors_a[0])
    );
    let q_b = format!(
        "SELECT id FROM table_b ORDER BY embedding <=> '{}' LIMIT 5",
        vec_literal(&vectors_b[0])
    );
    // `table_b` を 1 回問い合わせ、SQL 表層の結果・Rust API の結果とともに
    // HNSW 経路を通ったこと（`hnsw_path_count` の増加）を確認して返す。
    let observe_b = |label: &str| {
        let before = hnsw_path_count(&core);
        let sql = core
            .execute_sql(&ctx("tenant-a"), &q_b)
            .unwrap_or_else(|e| panic!("sql distance query on table_b ({label}): {e:?}"));
        let after_sql = hnsw_path_count(&core);
        assert!(
            after_sql > before,
            "table_b (sql, {label}) must use the hnsw path: {:?}",
            core.hnsw_index_cache_stats()
        );
        let api = rust_api_top_k(&core, "table_b", &vectors_b[0]);
        assert!(
            hnsw_path_count(&core) > after_sql,
            "table_b (rust api, {label}) must use the hnsw path: {:?}",
            core.hnsw_index_cache_stats()
        );
        (sql, api)
    };

    // 宣言前（カタログに宣言 0 件）: `table_b` は HNSW（宣言導入前の自動挙動）。
    let (sql_before, api_before) = observe_b("before declaration");
    assert_eq!(sql_before.rows.len(), 5);
    assert_eq!(api_before.len(), 5);

    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_vec_a ON table_a USING hnsw (embedding)",
    )
    .expect("declare hnsw index on table_a only");

    // 宣言した `table_a` 自身も HNSW 経路。
    let before_a = hnsw_path_count(&core);
    core.execute_sql(&ctx("tenant-a"), &q_a)
        .expect("distance query on declared table");
    assert!(
        hnsw_path_count(&core) > before_a,
        "declared table must use the hnsw path: {:?}",
        core.hnsw_index_cache_stats()
    );

    // `table_a` への宣言後も `table_b` の経路・結果は宣言前と同一。
    let (sql_after, api_after) = observe_b("after declaration on table_a");
    assert_eq!(
        sql_after, sql_before,
        "declaring an index on table_a must not change table_b sql results"
    );
    assert_eq!(
        api_after, api_before,
        "declaring an index on table_a must not change table_b rust api results"
    );

    // `DROP INDEX` 後も同一。
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, "DROP INDEX idx_vec_a")
        .expect("drop hnsw declaration");
    let (sql_dropped, api_dropped) = observe_b("after dropping the declaration");
    assert_eq!(sql_dropped, sql_before);
    assert_eq!(api_dropped, api_before);
}
