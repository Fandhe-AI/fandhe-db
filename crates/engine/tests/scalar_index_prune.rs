//! `sql::scalar_index::ScalarIndex` を SCALAR 事前フィルタの候補削減へ結線する
//! （Issue #474・前提 Issue #473）SQL 表層結合テスト。
//!
//! `tests/scalar_index_cache.rs`（Issue #473・構築とキャッシュのみ）と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、
//! `engine::tenant::insert_typed_row` による投入）で、`EngineCore::execute_sql`
//! （`sql::exec::execute_statement_with_cache` の候補削減結線を必ず経由する
//! production 経路）と `EngineCore::scalar_index_cache_stats()`（テナント ID・
//! 値を含まないカウンタのみの観測用 API）を突き合わせる。
//!
//! 検証する契約（Issue #474 のスコープ）:
//! 1. 索引対応述語（等価・前方一致・`id` 単純比較・その組合せ）を持つクエリで、
//!    初回（cold・全走査のみ。索引はこの回で構築される）と 2 回目以降
//!    （hot・索引を候補削減に消費）の結果が完全一致し、2 回目以降は
//!    `index_scans` が増える
//! 2. 索引非対応の残余述語（`Builtin`／`HINT ORDER` による DISTANCE 先行）は
//!    索引を一切消費せず（`index_scans` 不変）、結果は全走査と一致し続ける
//! 3. 選択度が閾値を超える述語では索引経路を使わず（`plain_scan_fallbacks` が
//!    増える）、結果は全走査と一致する
//! 4. 評価エラー（0 除算等）を起こす式述語を含むクエリは、索引対応述語を含んで
//!    いても従来どおり `22000` で拒否する（残余述語ありなので索引不使用）
//! 5. RLS: 他テナントの private 行は索引経路でも一切露出しない
//! 6. テーブル世代の進行後、索引は再構築され結果は一貫し続ける
//!
//! 加えて、列単位の索引対象除外（平均値長ゲート。Issue #632・
//! `sql::scalar_index::MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN`）が SQL 表層の
//! 述語評価に影響しないこと（除外列への述語は `FallbackNoIndex` として
//! plain scan へ縮退しつつ、結果自体は常に全走査と一致し続けること）を
//! 「除外される長文列（`body`）」を持つ専用スキーマ（`schema_with_body`）で
//! 別途検証する（下部「列単位の索引対象除外」節参照）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

#[allow(clippy::too_many_arguments)]
fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    embedding: [f32; 2],
    kind: &str,
    path: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(embedding.to_vec()),
            Value::Text(kind.to_string()),
            Value::Text(path.to_string()),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

/// 10 行（`id` 1..=10）: 偶数 `id` は `kind = 'a'`・`path` に `"even/"` 接頭辞、
/// 奇数 `id` は `kind = 'b'`・`path` に `"odd/"` 接頭辞。距離は `id` に単調な
/// ベクトルにし、`ORDER BY ... LIMIT 20`（全件超）で常に全一致行を取得できる
/// ようにする。
fn seed_ten_rows(storage: &Storage, tenant: &str) {
    let tenant_ctx = ctx(tenant);
    for id in 1..=10u64 {
        let (kind, path) = if id % 2 == 0 {
            ("a", format!("even/{id}"))
        } else {
            ("b", format!("odd/{id}"))
        };
        insert_row(
            storage,
            &tenant_ctx,
            id,
            [id as f32, 0.0],
            kind,
            &path,
            Visibility::Public,
        );
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

/// クエリを 2 回実行し（cold: 索引構築のみ・hot: 索引消費）、結果が完全一致
/// することを固定したうえで、hot 実行後の `index_scans` 増分を返す。
fn assert_cold_hot_equivalent(core: &EngineCore, tenant: &str, sql: &str) -> u64 {
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, tenant, sql);
    let hot = run(core, tenant, sql);
    assert_eq!(
        result_ids(&cold),
        result_ids(&hot),
        "cold/hot results must match exactly for: {sql}"
    );
    let after = core.scalar_index_cache_stats().index_scans;
    after.saturating_sub(before)
}

// --- 契約 1: 索引対応述語の cold/hot 等価性 ---------------------------------

#[test]
fn equality_predicate_cold_hot_equivalence_and_index_consumption() {
    let path = unique_db_path("scalar-index-prune-equality");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "an equality predicate must be consumed by the index on the hot path"
    );
    // オラクル: 偶数 id のみ。
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![2, 4, 6, 8, 10]);
}

#[test]
fn prefix_predicate_cold_hot_equivalence_and_index_consumption() {
    let path = unique_db_path("scalar-index-prune-prefix");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql =
        "SELECT id FROM docs WHERE path LIKE 'odd/%' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "a prefix predicate must be consumed by the index on the hot path"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![1, 3, 5, 7, 9]);
}

/// SQL-24・TASK-208、Issue #914: `LIKE` の一般形（中間一致）は二次索引が
/// 対応せず、索引が構築済み（他述語の索引消費で温まった状態）でも
/// `index_scans` を一切消費しないこと、かつ結果が全走査オラクル（`id + 0 > 0`
/// で強制的に残余述語を持たせた対照クエリ）と完全一致することを固定する
/// （索引の有無で結果が変わらない SQL-24 の契約）。
#[test]
fn like_general_form_never_consumes_index_and_matches_plain_scan_oracle() {
    let path = unique_db_path("scalar-index-prune-like-general-form");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // 索引を先に温める（等価述語クエリで構築）。
    let _ = run(
        &core,
        "tenant-a",
        "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20",
    );

    let sql =
        "SELECT id FROM docs WHERE path LIKE '%/1' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let control_sql = "SELECT id FROM docs WHERE path LIKE '%/1' AND id + 0 > 0 \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";

    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(
        after, before,
        "LIKE general form must never consume the scalar index even when it is already built"
    );
    assert_eq!(
        result_ids(&cold),
        result_ids(&hot),
        "cold/hot results must match exactly for the LIKE general form"
    );

    let control = run(&core, "tenant-a", control_sql);
    assert_eq!(
        result_ids(&hot),
        result_ids(&control),
        "LIKE general form must match the forced plain-scan oracle"
    );
    // オラクル: `path` は奇数 id が `odd/{id}`・偶数 id が `even/{id}`
    // （`seed_ten_rows`）。末尾が `/1` の値は `odd/1`（id=1）のみ
    // （`even/10` は `/10` で終わるため不一致）。
    assert_eq!(result_ids(&hot), vec![1]);
}

#[test]
fn id_range_predicate_cold_hot_equivalence_and_index_consumption() {
    let path = unique_db_path("scalar-index-prune-id-range");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `WHERE kind = 'a'` を含めるとメタデータ索引経路に固定されてしまうため、
    // `id` の単純比較のみを索引対応述語にする（`kind` を含まない裸の `WHERE`）。
    // 一部の行のみ選ぶことで既定の選択度閾値（1/2）を下回らせる（10 行中 3 行）。
    let sql = "SELECT id FROM docs WHERE id > 7 ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "an id range predicate must be consumed by the index on the hot path"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![8, 9, 10]);

    // 左右入替・`=`／`<`／`>=`／`<=` も同じオラクルに一致する（`sql::scalar_plan::
    // id_predicate_from_expr` の左右入替・`id_bounds` の演算子網羅は単体テストで
    // 別途固定済みのため、ここでは SQL 表層からの往復を代表 1 パターンずつ確認する）。
    for (sql, expected) in [
        (
            "SELECT id FROM docs WHERE 7 < id ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20",
            vec![8u64, 9, 10],
        ),
        (
            "SELECT id FROM docs WHERE id = 5 ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20",
            vec![5],
        ),
        (
            "SELECT id FROM docs WHERE id <= 3 ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20",
            vec![1, 2, 3],
        ),
    ] {
        let result = run(&core, "tenant-a", sql);
        assert_eq!(result_ids(&result), expected, "sql={sql}");
    }
}

#[test]
fn conjunction_of_metadata_and_id_predicates_cold_hot_equivalence() {
    let path = unique_db_path("scalar-index-prune-conjunction");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql =
        "SELECT id FROM docs WHERE kind = 'a' AND id > 5 ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "a conjunction of index-eligible predicates must be consumed by the index"
    );
    // 偶数 id かつ id > 5。
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![6, 8, 10]);
}

/// `ScalarIndex::resolve_candidates` は述語ごとの候補列を全件集めてから交差
/// する実装から、生成のたびにその場で累積へ交差する逐次方式へ変更した
/// （codex-review P1 対応・PR #601。累計候補保持量に上限がなかった問題への
/// 対応。モジュールドキュメント参照）。3 つ以上の索引対応述語（メタデータ 2 件
/// ＋ `id` 範囲 1 件）を AND で束ねても、交差の評価順に関わらず正しい積集合に
/// 一致することを固定する。
#[test]
fn conjunction_of_three_predicates_matches_all_oracle() {
    let path = unique_db_path("scalar-index-prune-conjunction-triple");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // kind = 'a'（偶数 id）∧ path LIKE 'even/%'（偶数 id と等価集合）∧
    // id > 4（id 5 以上）。積集合は偶数かつ 4 より大きい id。
    let sql = "SELECT id FROM docs WHERE kind = 'a' AND path LIKE 'even/%' AND id > 4 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "a 3-predicate conjunction must still be consumed by the index"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![6, 8, 10]);
}

/// 逐次交差の早期打ち切り（累積が空集合になった時点で残りの述語を評価しない）
/// が結果の正しさに影響しないことを固定する。`kind = 'a'`（偶数 id）と
/// `path LIKE 'odd/%'`（奇数 id）は互いに素なため、この時点で累積は必ず
/// 空集合になる。
#[test]
fn conjunction_with_disjoint_predicates_short_circuits_to_empty_result() {
    let path = unique_db_path("scalar-index-prune-conjunction-empty");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT id FROM docs WHERE kind = 'a' AND path LIKE 'odd/%' AND id > 1 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let result = run(&core, "tenant-a", sql);
    assert!(result_ids(&result).is_empty());
    // cold/hot でも一致し続ける（空集合を返す索引経路自体が壊れていないこと）。
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(index_scans > 0);
}

// --- 契約 2: 索引非対応形状は索引を消費しない -------------------------------

#[test]
fn residual_builtin_expr_never_consumes_index() {
    let path = unique_db_path("scalar-index-prune-residual-builtin");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `vec_norm(embedding)` は `VectorRef` を参照する残余述語のため、
    // `kind = 'a'`（索引対応）が同居していても全体が `PlainScan` へ縮退する。
    let sql = "SELECT id FROM docs WHERE kind = 'a' AND vec_norm(embedding) > 0 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(
        before, after,
        "a residual VectorRef predicate must never be consumed by the index"
    );
    assert_eq!(result_ids(&hot), vec![2, 4, 6, 8, 10]);
}

#[test]
fn distance_leading_hint_order_never_consumes_index() {
    let path = unique_db_path("scalar-index-prune-hint-order");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `HINT ORDER(RLS, DISTANCE, SCALAR)`: SCALAR 段は DISTANCE 段の後で事後
    // 適用される（`scalar_prefilter == false`）ため索引経路の対象外。
    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20 \
               HINT ORDER(RLS, DISTANCE, SCALAR)";
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(
        before, after,
        "DISTANCE-leading HINT ORDER must never consume the index"
    );
}

// --- 契約 3: 選択度切替 -----------------------------------------------------

#[test]
fn low_selectivity_predicate_falls_back_to_plain_scan_but_result_still_matches() {
    let path = unique_db_path("scalar-index-prune-selectivity");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `kind = 'a'` は 10 行中 5 行一致（比 1/2）。既定の選択度閾値
    // （`hits * 2 > row_count * 1`、すなわち一致比が厳密に 1/2 を超えると縮退）
    // をちょうど満たさないため索引経路が使われる契約自体は上の equality テストで
    // 固定済み。ここでは半数を超える一致（6/10）を作り、確実に縮退させる
    // （SQL 表層 `INSERT` 経由。書き込む行の可視性は常に `Private` 固定〔`sql::exec`
    // モジュールドキュメント参照〕だが `ctx("tenant-a")` は `Private` も許可する）。
    core.execute_insert_sql(
        &ctx("tenant-a"),
        "INSERT INTO docs (id, embedding, kind, path) \
         VALUES (11, '[11.0,0.0]', 'a', 'even/11') USING OPERATION_ID 'seed-11'",
    )
    .expect("insert extra matching row");
    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[20.0,0.0]' LIMIT 20";
    let before = core.scalar_index_cache_stats();
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    let after = core.scalar_index_cache_stats();
    assert!(
        after.plain_scan_fallbacks > before.plain_scan_fallbacks,
        "a predicate exceeding the selectivity threshold must fall back to a plain scan \
         (before={before:?}, after={after:?})"
    );
    // 選択度超過による縮退は「候補削減を使わなかった」ことそのものを意味する
    // ため、`index_scans` は増えていないことも合わせて固定する（`plain_scan_
    // fallbacks` の増加だけでは「索引経路を経由したが最終的に絞れなかった」
    // 可能性を排除できない）。
    assert_eq!(
        after.index_scans, before.index_scans,
        "a selectivity fallback must not also be counted as an index scan"
    );
    assert_eq!(result_ids(&hot), vec![2, 4, 6, 8, 10, 11]);
}

// --- 契約 4: エラー契約（残余述語ありは索引不使用のまま従来どおり fail-closed） --

#[test]
fn division_by_zero_in_residual_expr_still_fails_closed_with_22000() {
    let path = unique_db_path("scalar-index-prune-error-contract");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // 可視行はすべて `kind` が `'a'` か `'b'` のいずれかであり、`kind = 'a'` に
    // 一致する行（偶数 id）が必ず存在する。`matches_all` が先に評価され一致
    // する行にのみ式述語が届くため、0 除算が実際に評価されることを保証する
    // （`on_visible_row` は `matches_all` を式述語より先に評価する既存契約）。
    let sql = "SELECT id FROM docs WHERE kind = 'a' AND (1.0 / 0.0) > 0 \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let err = core
        .execute_sql(&ctx("tenant-a"), sql)
        .expect_err("division by zero must still be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- 契約 5: RLS ------------------------------------------------------------

#[test]
fn other_tenant_private_rows_never_leak_through_index_path() {
    let path = unique_db_path("scalar-index-prune-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    // 別テナントの private 行（同じ `kind = 'a'`。tenant-a からは絶対に見えない
    // はずの id）。
    insert_row(
        &storage,
        &ctx("tenant-b"),
        999,
        [1.0, 0.0],
        "a",
        "even/999",
        Visibility::Private,
    );
    let core = new_core(storage);

    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    // cold（索引構築）→ hot（索引消費）の両方で他テナント行が漏れないことを固定。
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert!(!result_ids(&cold).contains(&999));
    assert!(!result_ids(&hot).contains(&999));
    assert_eq!(result_ids(&hot), vec![2, 4, 6, 8, 10]);
}

// --- 契約 6: テーブル世代の進行 ---------------------------------------------

#[test]
fn index_rebuilds_after_generation_bump_and_results_stay_consistent() {
    let path = unique_db_path("scalar-index-prune-generation");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_equivalent(&core, "tenant-a", sql);
    let before_builds = core.scalar_index_cache_stats().builds;

    // 新規行を書き込み世代を進める（既存の `kind = 'a'` 一致集合を拡張する）。
    core.execute_insert_sql(
        &ctx("tenant-a"),
        "INSERT INTO docs (id, embedding, kind, path) \
         VALUES (12, '[12.0,0.0]', 'a', 'even/12') USING OPERATION_ID 'seed-12'",
    )
    .expect("insert row after generation bump");

    let after_write = run(&core, "tenant-a", sql);
    assert!(
        result_ids(&after_write).contains(&12),
        "the newly written row must be visible after the generation bump"
    );
    let after_builds = core.scalar_index_cache_stats().builds;
    assert!(
        after_builds > before_builds,
        "a write must invalidate the stale index and trigger a rebuild"
    );

    // 再構築後の索引でも cold/hot 等価性が維持される。
    assert_cold_hot_equivalent(&core, "tenant-a", sql);
}

// --- 列単位の索引対象除外（平均値長ゲート。Issue #632） --------------------
//
// `sql::scalar_index::ScalarIndex::build` は平均値長が閾値（本リポジトリの
// 実装既定値。`crates/engine/src/sql/scalar_index.rs::
// MAX_SCALAR_INDEX_COLUMN_AVG_TEXT_LEN` 参照）を超える `TEXT` 列を索引対象
// から除外する。除外は `candidates_for`/`column_groups` が `None` を返す
// 既存の「列が索引未対応」契約へ合流するだけであり、`sql::exec` はこれを
// `CandidateResolution::FallbackNoIndex` として plain scan へ縮退させる
// （SELECT を先に流さなくても集計・GROUP BY 経路〔Issue #475〕から piggyback
// で索引が構築される契約と同型で、除外判定自体は構築時に確定する）。

/// `embedding`／`kind`（短い分類値）／`body`（除外対象になる長文自由記述）を
/// 持つ専用スキーマ。既存 `schema()`／`insert_row` は非破壊のまま、本節専用に
/// 新設する（他の契約テストへの影響を避けるため）。
fn schema_with_body() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

#[allow(clippy::too_many_arguments)]
fn insert_row_with_body(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    embedding: [f32; 2],
    kind: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(embedding.to_vec()),
            Value::Text(kind.to_string()),
            Value::Text(body.to_string()),
        ],
        &op_id(&format!("seed-body-{id}")),
    )
    .expect("insert row");
}

/// 平均値長ゲート（Issue #632）で必ず除外される長さの `body` 値。閾値の
/// 具体値（本リポジトリの実装既定値）に依存しない安全マージンを取り、
/// `id` ごとに一意な接頭辞（`body-{id}-`）を持たせて前方一致述語の
/// テストでも `id` を一意に選別できるようにする。
fn long_body_value(id: u64) -> String {
    format!("body-{id}-{}", "x".repeat(1000))
}

/// 10 行（`id` 1..=10）: 偶数 `id` は `kind = 'a'`、奇数 `id` は `kind = 'b'`。
/// `body` は常に [`long_body_value`]（除外対象の長文）。距離は `id` に単調な
/// ベクトルにし、`ORDER BY ... LIMIT 20`（全件超）で常に全一致行を取得できる
/// ようにする（`seed_ten_rows` と対になる、`body` 列を持つ版）。
fn seed_ten_rows_with_long_body(storage: &Storage, tenant: &str) {
    let tenant_ctx = ctx(tenant);
    for id in 1..=10u64 {
        let kind = if id % 2 == 0 { "a" } else { "b" };
        insert_row_with_body(
            storage,
            &tenant_ctx,
            id,
            [id as f32, 0.0],
            kind,
            &long_body_value(id),
            Visibility::Public,
        );
    }
}

// --- シナリオ a: 除外列への等価述語 -----------------------------------------

#[test]
fn excluded_long_text_column_equality_predicate_matches_full_scan_and_skips_index() {
    let path = unique_db_path("scalar-index-prune-excluded-equality");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_body())
        .expect("create table");
    seed_ten_rows_with_long_body(&storage, "tenant-a");
    let core = new_core(storage);

    let target = long_body_value(3);
    let sql = format!(
        "SELECT id FROM docs WHERE body = '{target}' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20"
    );
    let before = core.scalar_index_cache_stats();
    let cold = run(&core, "tenant-a", &sql);
    let hot = run(&core, "tenant-a", &sql);
    assert_eq!(
        result_ids(&cold),
        result_ids(&hot),
        "cold/hot results must match exactly even when the predicate column is excluded"
    );
    assert_eq!(result_ids(&hot), vec![3]);
    let after = core.scalar_index_cache_stats();
    assert!(
        after.plain_scan_fallbacks > before.plain_scan_fallbacks,
        "an equality predicate on an excluded (too-long) column must fall back to a plain scan"
    );
    assert_eq!(
        after.index_scans, before.index_scans,
        "an excluded column must never be counted as an index scan"
    );
}

// --- シナリオ b: 除外列への前方一致述語 -------------------------------------

#[test]
fn excluded_long_text_column_prefix_predicate_matches_full_scan_and_skips_index() {
    let path = unique_db_path("scalar-index-prune-excluded-prefix");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_body())
        .expect("create table");
    seed_ten_rows_with_long_body(&storage, "tenant-a");
    let core = new_core(storage);

    // `body-1-` は `id=1` の値にのみ前方一致する（`id=10` は `body-10-` で
    // 4 文字目が異なる）。
    let sql = "SELECT id FROM docs WHERE body LIKE 'body-1-%' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let before = core.scalar_index_cache_stats();
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    assert_eq!(result_ids(&hot), vec![1]);
    let after = core.scalar_index_cache_stats();
    assert!(
        after.plain_scan_fallbacks > before.plain_scan_fallbacks,
        "a prefix predicate on an excluded (too-long) column must fall back to a plain scan"
    );
    assert_eq!(
        after.index_scans, before.index_scans,
        "an excluded column must never be counted as an index scan"
    );
}

// --- シナリオ c: 短い列は除外の影響を受けず索引を消費し続ける ---------------

#[test]
fn short_text_column_still_uses_index_alongside_excluded_long_column() {
    let path = unique_db_path("scalar-index-prune-excluded-short-unaffected");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_body())
        .expect("create table");
    seed_ten_rows_with_long_body(&storage, "tenant-a");
    let core = new_core(storage);

    // `body` 列（除外対象）を含まない `kind` 単独述語は、`body` 列の除外の
    // 有無に関わらず従来どおり索引を消費する（回帰確認）。
    let sql = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "excluding one column must not prevent another indexed column from being used"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![2, 4, 6, 8, 10]);
}

// --- シナリオ d: 短い列＋除外列の複合述語は索引経路を一切使わない -----------

#[test]
fn compound_predicate_with_excluded_column_skips_index_entirely() {
    let path = unique_db_path("scalar-index-prune-excluded-compound");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_body())
        .expect("create table");
    seed_ten_rows_with_long_body(&storage, "tenant-a");
    let core = new_core(storage);

    // `kind` は索引対応述語だが、`body` の等価述語で `candidates_for` が
    // `None` を返すため `resolve_candidates` は述語 1 つでも `None` に
    // 遭遇した時点で全体を `FallbackNoIndex` にする既存実装（モジュール
    // ドキュメント「消費経路」参照）。索引経路を一切使わず、結果は全走査と
    // 一致する。
    let target = long_body_value(4);
    let sql = format!(
        "SELECT id FROM docs WHERE kind = 'a' AND body = '{target}' \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20"
    );
    let before = core.scalar_index_cache_stats();
    let cold = run(&core, "tenant-a", &sql);
    let hot = run(&core, "tenant-a", &sql);
    assert_eq!(result_ids(&cold), result_ids(&hot));
    assert_eq!(result_ids(&hot), vec![4]);
    let after = core.scalar_index_cache_stats();
    assert!(
        after.plain_scan_fallbacks > before.plain_scan_fallbacks,
        "a compound predicate referencing an excluded column must fall back to a plain scan"
    );
    assert_eq!(
        after.index_scans, before.index_scans,
        "a compound predicate referencing an excluded column must never use the index"
    );
}

// --- シナリオ e: RLS ---------------------------------------------------------

#[test]
fn excluded_column_predicate_never_leaks_other_tenant_private_rows() {
    let path = unique_db_path("scalar-index-prune-excluded-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema_with_body())
        .expect("create table");
    seed_ten_rows_with_long_body(&storage, "tenant-a");
    // 別テナントの private 行。`body` は tenant-a の `id=3` 行と全く同じ値
    // （除外列の値が RLS 判定をバイパスしないことを固定する）。
    insert_row_with_body(
        &storage,
        &ctx("tenant-b"),
        999,
        [3.0, 0.0],
        "a",
        &long_body_value(3),
        Visibility::Private,
    );
    let core = new_core(storage);

    let target = long_body_value(3);
    let sql = format!(
        "SELECT id FROM docs WHERE body = '{target}' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20"
    );
    let cold = run(&core, "tenant-a", &sql);
    let hot = run(&core, "tenant-a", &sql);
    assert!(!result_ids(&cold).contains(&999));
    assert!(!result_ids(&hot).contains(&999));
    assert_eq!(result_ids(&hot), vec![3]);
}
