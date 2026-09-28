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
//! - HNSW 適格性ゲートはテーブル単位（`docs/design/index-declaration-effects.md`
//!   「HNSW（テーブル単位）」）: `HnswScope::All`（既定）では全テーブル HNSW で
//!   宣言の追加・削除はどのテーブルの経路・Top-k も変えず、`HnswScope::Declared`
//!   では `USING hnsw` を宣言したテーブルだけ HNSW・未宣言テーブルは厳密で
//!   `DROP INDEX` で厳密へ戻ること。いずれも `table_a` への宣言は `table_b` の
//!   経路・Top-k（SQL 表層・Rust API・`EXPLAIN` の `ann_plan:`）を変えないこと
//! - HNSW opt-in なしでは `HnswScope` は無関係で全テーブル厳密のままであること
//! - いずれの構成でも RLS 境界（テナント間非漏えい）は変わらないこと

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine::{self, HnswScope};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
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

/// `HnswIndexCache` へ照会した回数の合計（HNSW 適格性ゲートを通過して HNSW
/// 経路へ進むと、新規構築・索引済み探索・世代更新後のオーバーレイ再計算
/// 〔`misses` に計上〕のいずれかで必ず増える）。ゲート対象外（厳密
/// brute-force）では一切増えない。
fn hnsw_consulted_count(core: &EngineCore) -> u64 {
    let stats = core.hnsw_index_cache_stats();
    stats.hits + stats.misses + stats.builds + stats.build_failures + stats.fallbacks
}

/// HNSW opt-in の `EngineCore` を `scope` で構築し、同じスキーマの `table_a`・
/// `table_b`（いずれも `HNSW_ROWS` 行・宣言なし）を用意する。
fn two_table_hnsw_core(
    label: &str,
    scope: HnswScope,
) -> (EngineCore, CleanupGuard, Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
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
    let core = EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(scope);
    assert_eq!(core.hnsw_scope(), scope);
    (core, guard, vectors_a, vectors_b)
}

/// `table` への DISTANCE 検索 1 回分の観測結果。
#[derive(Debug, PartialEq)]
struct Observed {
    /// SQL 表層（`execute_sql`）の結果。
    sql: engine::sql::exec::QueryResult,
    /// Rust API（`VectorCore::search`）の Top-k（`(tenant, id, score bits)`）。
    api: Vec<(String, u64, u32)>,
    /// `EXPLAIN` の `ann_plan:` 行。
    ann_plan: String,
}

/// `table` を SQL 表層・Rust API・`EXPLAIN` の 3 経路で 1 回ずつ問い合わせ、
/// SQL 表層・Rust API がそれぞれ HNSW 経路を使った（`expect_hnsw == true`）／
/// 使わなかった（`false`。`HnswIndexCache` へ一切照会しない）ことを確認して
/// 観測結果を返す。戻り値の 2 つ目は `ann_plan:` の生の行（索引名注記
/// 〔Issue #1066〕を含む）。`Observed`（1 つ目）はトークン部分のみを持ち、
/// 索引名注記の有無に関わらず経路・Top-k が不変であることの比較
/// （`assert_eq!(a_declared, a_before)` 等）に使う。
fn observe(
    core: &EngineCore,
    table: &str,
    q: &[f32],
    expect_hnsw: bool,
    label: &str,
) -> (Observed, String) {
    let sql_text = format!(
        "SELECT id FROM {table} ORDER BY embedding <=> '{}' LIMIT 5",
        vec_literal(q)
    );
    let check = |before_consulted: u64, surface: &str| {
        let stats = core.hnsw_index_cache_stats();
        if expect_hnsw {
            assert!(
                hnsw_consulted_count(core) > before_consulted,
                "{label}: {table} ({surface}) must use the hnsw path: {stats:?}"
            );
        } else {
            assert_eq!(
                hnsw_consulted_count(core),
                before_consulted,
                "{label}: {table} ({surface}) must use exact brute-force: {stats:?}"
            );
        }
    };

    let c = hnsw_consulted_count(core);
    let sql = core
        .execute_sql(&ctx("tenant-a"), &sql_text)
        .unwrap_or_else(|e| panic!("{label}: sql distance query on {table}: {e:?}"));
    check(c, "sql");

    let c = hnsw_consulted_count(core);
    let api: Vec<(String, u64, u32)> = core
        .search(&ctx("tenant-a"), table, q, 5)
        .unwrap_or_else(|e| panic!("{label}: rust api search on {table}: {e:?}"))
        .into_iter()
        .map(|h| (h.tenant_id, h.id, h.score.to_bits()))
        .collect();
    check(c, "rust api");

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            &format!("EXPLAIN {sql_text}"),
        )
        .unwrap_or_else(|e| panic!("{label}: explain on {table}: {e:?}"));
    let ann_plan_line = match outcome {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .find_map(|row| match row.cells.first() {
                Some(Cell::Text(s)) if s.starts_with("ann_plan: ") => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{label}: explain must report an ann_plan: line")),
        other => panic!("{label}: expected SqlOutcome::Explain, got {other:?}"),
    };
    // Issue #1066: `ann_plan:` 行は索引経路使用時 ` index=<name>[,<name>...]`
    // を条件付きで追記する（安定契約。既存トークンの末尾への追記のみ）。
    // トークン部分だけを切り出し、経路・Top-k 比較（`Observed`）には索引名
    // 注記の有無を混ぜない。
    let ann_plan = ann_plan_line
        .split(" index=")
        .next()
        .expect("str::split always yields at least one segment")
        .to_string();
    let expected_plan = if expect_hnsw {
        "ann_plan: hnsw_full_visible"
    } else {
        "ann_plan: plain_scan_engine"
    };
    assert_eq!(ann_plan, expected_plan, "{label}: explain for {table}");

    assert_eq!(sql.rows.len(), 5);
    assert_eq!(api.len(), 5);
    (Observed { sql, api, ann_plan }, ann_plan_line)
}

/// `--hnsw-scope all`（既定。`HnswScope::All`）: opt-in 時は全テーブル HNSW で、
/// `table_a` に HNSW 宣言を追加・削除しても、宣言のない `table_b` の検索経路
/// （HNSW）と Top-k 結果（SQL 表層・Rust API・`EXPLAIN` の `ann_plan:`）が一切
/// 変わらないことを固定する（Issue #1065 の「クエリ結果が宣言の有無で変わら
/// ない」受け入れ条件。旧カタログ全体単位ゲートでは `table_a` への宣言で
/// `table_b` が近似〔HNSW〕から厳密〔brute-force〕へ切り替わり Top-k が変わり
/// 得た回帰の防止）。宣言した `table_a` 自身も宣言前と同じく HNSW のまま。
/// `HnswScope::All` は適格性が宣言に依存しないため、宣言後も索引名注記は
/// 付かない（Issue #1066 PR #1155・codex-review P1 指摘対応）。
#[test]
fn scope_all_declaration_on_one_table_does_not_change_any_table_results_or_path() {
    let (core, _guard, vectors_a, vectors_b) =
        two_table_hnsw_core("index-decl-hnsw-scope-all", HnswScope::All);
    // `with_hnsw_scope` を呼ばない構築の既定値も `All`。
    assert_eq!(HnswScope::default(), HnswScope::All);
    let mut session = allowed_session();

    let (a_before, a_before_line) = observe(&core, "table_a", &vectors_a[0], true, "before");
    let (b_before, b_before_line) = observe(&core, "table_b", &vectors_b[0], true, "before");
    // Issue #1066: 宣言前はどのテーブルにも索引名注記が無い。
    assert!(!a_before_line.contains("index="), "{a_before_line}");
    assert!(!b_before_line.contains("index="), "{b_before_line}");

    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_vec_a ON table_a USING hnsw (embedding)",
    )
    .expect("declare hnsw index on table_a only");
    let (a_declared, a_declared_line) = observe(&core, "table_a", &vectors_a[0], true, "declared");
    let (b_declared, b_declared_line) = observe(&core, "table_b", &vectors_b[0], true, "declared");
    assert_eq!(
        a_declared, a_before,
        "scope all: table_a results must not change"
    );
    assert_eq!(
        b_declared, b_before,
        "scope all: a declaration on table_a must not change table_b results"
    );
    // codex-review P1 指摘対応（Issue #1066 PR #1155）: `HnswScope::All` は
    // `hnsw_targeted_in_txn` が宣言の有無を見ず常に適格とするため、宣言の
    // 追加・削除は経路を一切変えない（上記アサーションのとおり）。索引名
    // 注記は「実際に使われた索引」を示す契約であり、この scope では宣言が
    // 経路選択に無関係なため、宣言済みの table_a にも索引名を付けない。
    assert!(
        !a_declared_line.contains("index="),
        "scope all: a declared hnsw index must not be reported as used ({a_declared_line})"
    );
    assert!(
        !b_declared_line.contains("index="),
        "table_b must not report an index name for an undeclared table: {b_declared_line}"
    );

    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, "DROP INDEX idx_vec_a")
        .expect("drop hnsw declaration");
    let (a_dropped, a_dropped_line) = observe(&core, "table_a", &vectors_a[0], true, "dropped");
    let (b_dropped, b_dropped_line) = observe(&core, "table_b", &vectors_b[0], true, "dropped");
    assert_eq!(a_dropped, a_before);
    assert_eq!(b_dropped, b_before);
    // Issue #1066: `DROP INDEX` 後は索引名注記が消える。
    assert!(!a_dropped_line.contains("index="), "{a_dropped_line}");
    assert!(!b_dropped_line.contains("index="), "{b_dropped_line}");
}

/// `--hnsw-scope declared`（`HnswScope::Declared`）: `CREATE INDEX ... USING
/// hnsw` を宣言した `table_a` だけが HNSW 経路（SQL 表層・Rust API・`EXPLAIN`
/// の `ann_plan:` とも）を使い、未宣言の `table_b` は宣言の前後を通じて厳密
/// （brute-force）のまま経路・結果が一切変わらないこと、`DROP INDEX` で
/// `table_a` が厳密へ戻り宣言前と同一の結果を返すことを固定する（Issue #1065・
/// オーナー判断 2026-09-28。判定はテーブル単位）。
#[test]
fn scope_declared_uses_hnsw_only_on_declared_table_and_drop_returns_to_exact() {
    let (core, _guard, vectors_a, vectors_b) =
        two_table_hnsw_core("index-decl-hnsw-scope-declared", HnswScope::Declared);
    let mut session = allowed_session();

    // 宣言前: どのテーブルも厳密。
    let (a_before, a_before_line) = observe(&core, "table_a", &vectors_a[0], false, "before");
    let (b_before, b_before_line) = observe(&core, "table_b", &vectors_b[0], false, "before");
    assert!(!a_before_line.contains("index="), "{a_before_line}");
    assert!(!b_before_line.contains("index="), "{b_before_line}");

    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_vec_a ON table_a USING hnsw (embedding)",
    )
    .expect("declare hnsw index on table_a only");
    // 宣言した `table_a` だけ HNSW（宣言テーブル自身の経路は変わる）。
    // Issue #1066: 宣言テーブルの `ann_plan:` には索引名が付く。
    let (_, a_declared_line) = observe(&core, "table_a", &vectors_a[0], true, "declared");
    assert_eq!(
        a_declared_line, "ann_plan: hnsw_full_visible index=idx_vec_a",
        "table_a must report its declared hnsw index name"
    );
    // 未宣言の `table_b` は厳密のままで、結果も宣言前と同一（索引名も付かない）。
    let (b_declared, b_declared_line) = observe(&core, "table_b", &vectors_b[0], false, "declared");
    assert_eq!(
        b_declared, b_before,
        "scope declared: a declaration on table_a must not change table_b"
    );
    assert!(
        !b_declared_line.contains("index="),
        "table_b must not report an index name for an undeclared table: {b_declared_line}"
    );

    // `DROP INDEX` で `table_a` は厳密へ戻り、宣言前と同一の結果を返す。
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, "DROP INDEX idx_vec_a")
        .expect("drop hnsw declaration");
    let (a_dropped, a_dropped_line) = observe(&core, "table_a", &vectors_a[0], false, "dropped");
    assert_eq!(
        a_dropped, a_before,
        "scope declared: drop index must return table_a to exact search"
    );
    assert!(!a_dropped_line.contains("index="), "{a_dropped_line}");
    let (b_dropped, b_dropped_line) = observe(&core, "table_b", &vectors_b[0], false, "dropped");
    assert_eq!(b_dropped, b_before);
    assert!(!b_dropped_line.contains("index="), "{b_dropped_line}");
}

/// HNSW opt-in なし（既定エンジン）では `--hnsw-scope` は無関係で、
/// `HnswScope::Declared` を設定して `USING hnsw` を宣言しても全テーブル厳密の
/// まま（`HnswIndexCache` を一切使わない）ことを固定する。
#[test]
fn hnsw_scope_has_no_effect_without_hnsw_opt_in() {
    let path = unique_db_path("index-decl-hnsw-scope-no-optin");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_vector("table_a"))
        .expect("create table_a");
    let vectors_a = gen_vectors(5, DIM as usize, HNSW_ROWS);
    seed_rows(&storage, "table_a", "tenant-a", &vectors_a, "base");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_hnsw_scope(HnswScope::Declared);
    let mut session = allowed_session();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_vec_a ON table_a USING hnsw (embedding)",
    )
    .expect("declare hnsw index");
    core.execute_sql(
        &ctx("tenant-a"),
        &format!(
            "SELECT id FROM table_a ORDER BY embedding <=> '{}' LIMIT 5",
            vec_literal(&vectors_a[0])
        ),
    )
    .expect("distance query");
    core.search(&ctx("tenant-a"), "table_a", &vectors_a[0], 5)
        .expect("rust api search");
    assert_eq!(
        hnsw_consulted_count(&core),
        0,
        "without hnsw opt-in the scope must not enable hnsw"
    );
}
