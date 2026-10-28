//! RLS 検索経路（`engine::rls::PrefilterIndex` / `engine::rls::SearchTimeFilter`）に対する
//! セキュリティ機械検証の回帰テスト（TASK-135・対象ビヘイビア: RLS-1）。
//!
//! `tests/rls_prefilter.rs`（TASK-133）は各方式の契約検証が中心だが、本ファイルは
//! 「複数可視率 × 複数テナント視点 × 複数 k を横断し、検索結果への不許可行の混入 0 件を
//! 実装非依存のチェッカーで機械検証する」ことに主眼を置く。オラクル（許可集合）は
//! [`crate::policy::PolicyContext::is_visible`] を一切呼ばず、シード時にテスト側で
//! 独立に記録する（本ファイルの `is_allowed` 参照。production の判定関数をオラクルへ
//! 流用すると、その関数自体のバグを検出できなくなるため）。
//!
//! 決定的擬似乱数（xorshift64*）・`unique_db_path` + `CleanupGuard`・`Storage::open` での
//! 直接シードは `tests/rls_prefilter.rs` の流儀を踏襲する。

use std::collections::{BTreeSet, HashMap};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::kernel::{CpuScalarProvider, SearchHit};
use engine::policy::PolicyContext;
use engine::rls::{PrefilterIndex, SearchTimeFilter};
use engine::storage::{RowInput, Storage, Visibility};

// ---------- 決定的擬似乱数（xorshift64*。`tests/rls_prefilter.rs` と同一実装） ----------

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

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn next_f32_signed(&mut self) -> f32 {
        (self.next_f64() * 2.0 - 1.0) as f32
    }

    /// `choices` から決定的に 1 要素を選ぶ（テナント割り当てに使う）。
    fn choose<'a, T>(&mut self, choices: &'a [T]) -> &'a T {
        let idx = (self.next_u64() as usize) % choices.len();
        &choices[idx]
    }
}

// ---------- テスト共通のセットアップ ----------
// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した。

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// テナント単位へ分割してシード投入する共通ヘルパ（複数テストへの複製を避けるため
// `src/test_util/seed_rows.rs` へ一本化した。`temp_db.rs` と同じ取り込み方式）。
use crate::seed_rows;
use seed_rows::seed_rows_grouped_by_tenant;

const DIM: u32 = 12;
const TABLE: &str = "docs";

fn schema_for(table_name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        table_name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
    )
}

/// シード時に記録する 1 行分の真値（production コードの `is_visible` を経由せず、
/// テスト側で保持する独立レコード）。
#[derive(Clone, Copy)]
struct RowTruth {
    tenant: &'static str,
    visibility: Visibility,
}

/// [`PolicyContext::is_visible`] を呼ばずに、テスト側で独立に可視性を判定するオラクル。
/// production の判定ロジック（TABLE-9 のポインタ表記対象）と同じ契約を、シード時に
/// 記録した真値のみから再計算する（`is_visible` 自体のバグも検出できるようにするため、
/// 本関数は production コードを一切呼ばない）。
fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => allow_private && row.tenant == viewer_tenant,
    }
}

/// 複数テナントにまたがる決定的コーパスを構築し、行ごとの真値を id → [`RowTruth`] で返す。
/// `tenants` の各要素は `(name, private_rate)`（その行の visibility を `Private` にする
/// 確率）で、テナントごとに Public/Private の混在比率を変えられるようにする
/// （k バリエーション・可視行 0 件テナントのケースを同一コーパス内で作るため）。
fn seed_multi_tenant_corpus(
    storage: &Storage,
    num_rows: u64,
    tenants: &[(&'static str, f64)],
    seed: u64,
) -> HashMap<u64, RowTruth> {
    storage
        .create_table(&schema_for(TABLE, DIM))
        .expect("create table");
    let mut rng = Xorshift64::new(seed);
    let tenant_names: Vec<&'static str> = tenants.iter().map(|(n, _)| *n).collect();
    let mut truth: HashMap<u64, RowTruth> = HashMap::with_capacity(num_rows as usize);
    let mut rows: Vec<(u64, RowInput<'_>)> = Vec::with_capacity(num_rows as usize);
    let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(num_rows as usize);

    for id in 1..=num_rows {
        let tenant = *rng.choose(&tenant_names);
        let private_rate = tenants
            .iter()
            .find(|(n, _)| *n == tenant)
            .map(|(_, r)| *r)
            .unwrap_or(0.0);
        let visibility = if rng.next_f64() < private_rate {
            Visibility::Private
        } else {
            Visibility::Public
        };
        truth.insert(id, RowTruth { tenant, visibility });
        embeddings.push((0..DIM).map(|_| rng.next_f32_signed()).collect());
    }
    for id in 1..=num_rows {
        let idx = (id - 1) as usize;
        let row_truth = truth[&id];
        rows.push((
            id,
            RowInput {
                tenant_id: row_truth.tenant,
                visibility: row_truth.visibility,
                embedding: &embeddings[idx],
                metadata: &[],
            },
        ));
    }
    seed_rows_grouped_by_tenant(storage, TABLE, &rows);
    truth
}

/// `truth` から `viewer_tenant`（`allow_private` 付き）の許可 id 集合を独立に算出する。
fn allowed_ids(
    truth: &HashMap<u64, RowTruth>,
    viewer_tenant: &str,
    allow_private: bool,
) -> BTreeSet<u64> {
    truth
        .iter()
        .filter(|(_, t)| is_allowed(t, viewer_tenant, allow_private))
        .map(|(id, _)| *id)
        .collect()
}

fn random_query(seed: u64) -> Vec<f32> {
    let mut rng = Xorshift64::new(seed);
    (0..DIM).map(|_| rng.next_f32_signed()).collect()
}

/// `tenant_count` テナント均等割り当ての下で目標可視率 `visible_rate` を実際に
/// 達成するために各テナントへ一律適用すべき private_rate を算出する。
///
/// viewer の実可視率は「Public 行（全テナント共通で可視）」と「Private 行のうち
/// 自テナント分（`1/tenant_count` の確率で自テナントに属する）」の合算になる:
/// `visible_rate = public_rate + (1 - public_rate) / tenant_count`。
/// これを `public_rate` について解くと `(visible_rate * tenant_count - 1) / (tenant_count - 1)`
/// となる（codex-review 指摘: 旧実装は `private_rate = 1 - visible_rate` を 2 テナントへ
/// そのまま適用しており、Private 行の約半数が viewer 自身の行になる分だけ実可視率が
/// 目標より高くなっていた。低可視率 10% を実際に生成するには `tenant_count >= 10` が要る）。
fn private_rate_for_visible_rate(visible_rate: f64, tenant_count: usize) -> f64 {
    let t = tenant_count as f64;
    let public_rate = ((visible_rate * t - 1.0) / (t - 1.0)).clamp(0.0, 1.0);
    1.0 - public_rate
}

/// 目標可視率マトリクステスト（`rls1_prefilter_...`/`rls1_search_time_filter_...`）で
/// 共有するテナント名一覧。10 テナント均等割り当てで初めて可視率 10% を正確に生成できる
/// （[`private_rate_for_visible_rate`] のコメント参照）。
const RATE_MATRIX_TENANT_NAMES: [&str; 10] = [
    "tenant-a", "tenant-b", "tenant-c", "tenant-d", "tenant-e", "tenant-f", "tenant-g", "tenant-h",
    "tenant-i", "tenant-j",
];

/// 許可集合の実測可視率が目標 `visible_rate` に近いことを許容誤差内で検証する
/// （codex-review 指摘対応: 生成方法を変えただけでは目標可視率を実際に生成できて
/// いる保証にならないため、実測比率を assertion で担保する）。
fn assert_visible_ratio_close_to_target(
    allowed_len: usize,
    total_rows: u64,
    visible_rate: f64,
    viewer: &str,
    rate_idx: usize,
) {
    const TOLERANCE: f64 = 0.03;
    let actual = allowed_len as f64 / total_rows as f64;
    assert!(
        (actual - visible_rate).abs() <= TOLERANCE,
        "generated corpus did not realize the intended visible rate \
         (viewer={viewer}, rate_idx={rate_idx}, target={visible_rate}, actual={actual})"
    );
}

// ---------- 機械チェッカー ----------

/// 検索結果 `hits` を許可集合 `allowed` に対して検査し、違反件数を返す。
/// 検査項目: (1) 許可集合外の id の混入、(2) id の重複、(3) 要求件数 `k` の超過。
/// 実装（`PrefilterIndex`/`SearchTimeFilter`）に依存しない独立関数として、
/// `tests/rls_security.rs` 内の全マトリクステストから共通利用する。
fn count_policy_violations(hits: &[SearchHit], allowed: &BTreeSet<u64>, k: usize) -> usize {
    let mut violations = 0usize;
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    for hit in hits {
        if !allowed.contains(&hit.id) {
            violations += 1;
        }
        if !seen.insert(hit.id) {
            violations += 1;
        }
    }
    if hits.len() > k {
        violations += 1;
    }
    violations
}

// 対象ビヘイビア: RLS-1。チェッカー自体の健全性テスト（vacuous test 防止）。
// 不許可 id を含む捏造結果・重複 id・k 超過をそれぞれチェッカーへ与え、各違反が
// 非 0 件で検出されることを検証する（チェッカーが常に 0 を返す退化を防ぐ
// ネガティブコントロール）。
#[test]
fn checker_negative_control_detects_fabricated_violations() {
    let allowed: BTreeSet<u64> = [1, 2, 3].into_iter().collect();

    // 正常系: 全ヒットが許可集合内・重複なし・k 以内 → 違反 0 件。
    let clean_hits = vec![
        SearchHit::new("tenant-a", 1, 1.0),
        SearchHit::new("tenant-a", 2, 0.5),
    ];
    assert_eq!(count_policy_violations(&clean_hits, &allowed, 5), 0);

    // 不許可 id の混入。
    let leaked_hits = vec![
        SearchHit::new("tenant-a", 1, 1.0),
        SearchHit::new("tenant-a", 999, 0.9),
    ];
    assert!(
        count_policy_violations(&leaked_hits, &allowed, 5) > 0,
        "checker must flag an id outside the allowed set"
    );

    // 重複 id。
    let duplicate_hits = vec![
        SearchHit::new("tenant-a", 1, 1.0),
        SearchHit::new("tenant-a", 1, 1.0),
    ];
    assert!(
        count_policy_violations(&duplicate_hits, &allowed, 5) > 0,
        "checker must flag a duplicate id"
    );

    // k 超過（k=1 に対し 2 件返却）。
    assert!(
        count_policy_violations(&clean_hits, &allowed, 1) > 0,
        "checker must flag a result count exceeding k"
    );
}

/// 検索結果の件数が「不許可行で埋めていない」ことを厳密に検証する。
/// `k <= allowed.len()` なら `hits.len() == k`（充足できるのに不足させない）、
/// `k > allowed.len()` なら `hits.len() == allowed.len()`（可視行数で頭打ちになり、
/// 不許可行を混ぜて件数を水増ししない）ことを assert する。
fn assert_result_count_matches_visible_ceiling(hits: &[SearchHit], allowed_len: usize, k: usize) {
    let expected = allowed_len.min(k);
    assert_eq!(
        hits.len(),
        expected,
        "result count must be min(k, visible-row count) without padding from disallowed rows \
         (k={k}, allowed_len={allowed_len})"
    );
}

// ---------- 本体テスト: PrefilterIndex ----------

// 対象ビヘイビア: RLS-1。可視率（viewer の実測可視率。90% / 50% / 10%）×
// 複数テナント視点（先頭 2 テナント）× 複数 k（可視行数未満・可視行数超過）を横断し、
// `PrefilterIndex::search` の全試行で不許可行の混入 0 件を検証する。各 viewer は
// `PolicyContext::with_visibilities([Public, Private])` で自テナントの Private 行を
// 要求するため、他テナントの Private 行が混入すれば `row_tenant == self.tenant_id` の
// 判定が壊れていない限り検出できる（テナント境界の判定分岐そのものを踏む構成。
// Public のみ許可の既定 ctx では全テナント共通で可視になり判定分岐を踏まないため
// 採用しない）。10 テナント均等割り当てにして低可視率 10% の経路も実際に生成する
// （[`private_rate_for_visible_rate`] 参照。codex-review 指摘対応）。
#[test]
fn rls1_prefilter_no_violations_across_rate_tenant_k_matrix() {
    const NUM_ROWS: u64 = 3_000;
    const TENANT_COUNT: usize = RATE_MATRIX_TENANT_NAMES.len();

    for (rate_idx, &visible_rate) in [0.9, 0.5, 0.1].iter().enumerate() {
        let private_rate = private_rate_for_visible_rate(visible_rate, TENANT_COUNT);
        let tenants: Vec<(&str, f64)> = RATE_MATRIX_TENANT_NAMES
            .iter()
            .map(|&name| (name, private_rate))
            .collect();

        let path = unique_db_path(&format!("rls1-pf-{rate_idx}"));
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let seed = 5000 + rate_idx as u64;
        let truth = seed_multi_tenant_corpus(&storage, NUM_ROWS, &tenants, seed);

        for &(viewer, _) in &tenants[..2] {
            let ctx =
                PolicyContext::with_visibilities(viewer, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant");
            let allowed = allowed_ids(&truth, viewer, true);
            assert_visible_ratio_close_to_target(
                allowed.len(),
                NUM_ROWS,
                visible_rate,
                viewer,
                rate_idx,
            );

            let index = PrefilterIndex::build(&storage, TABLE, &ctx).expect("build index");
            assert_eq!(
                index.len(&ctx).expect("len ok"),
                allowed.len(),
                "prefilter index visible-row count must match the independent oracle \
                 (viewer={viewer}, rate_idx={rate_idx})"
            );

            for &k in &[5usize, allowed.len() + 50] {
                for query_idx in 0..3u64 {
                    let query = random_query(9000 + rate_idx as u64 * 100 + query_idx);
                    let hits = index
                        .search(&ctx, &CpuScalarProvider, &query, k)
                        .expect("search ok");
                    let violations = count_policy_violations(&hits, &allowed, k);
                    assert_eq!(
                        violations, 0,
                        "PrefilterIndex leaked disallowed rows (viewer={viewer}, k={k}, \
                         rate_idx={rate_idx}, query={query_idx})"
                    );
                    assert_result_count_matches_visible_ceiling(&hits, allowed.len(), k);
                }
            }
        }
    }
}

// 対象ビヘイビア: RLS-1。可視行 0 件テナント（自身の行を一切持たず、コーパス全行が
// 他テナントの Private である）でも拒否ではなく空結果を返す（fail-closed だが
// 過剰拒否ではないことのガード）。`row_tenant == self.tenant_id` の判定分岐が
// 他テナント行を弾いていることを、可視行 0 件という形で検証する。
#[test]
fn rls1_prefilter_zero_visible_tenant_returns_empty_without_error() {
    const NUM_ROWS: u64 = 500;
    // 全行を Private にする（Public 行があると zero-tenant にも見えてしまうため）。
    let tenants: [(&str, f64); 2] = [("tenant-a", 1.0), ("tenant-b", 1.0)];

    let path = unique_db_path("rls1-pf-zero");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_multi_tenant_corpus(&storage, NUM_ROWS, &tenants, 5500);

    let ctx_zero =
        PolicyContext::with_visibilities("zero-tenant", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let index = PrefilterIndex::build(&storage, TABLE, &ctx_zero).expect("build index");
    assert_eq!(index.len(&ctx_zero).expect("len ok"), 0);
    assert!(index.is_empty(&ctx_zero).expect("is_empty ok"));

    let hits = index
        .search(&ctx_zero, &CpuScalarProvider, &random_query(5501), 20)
        .expect("search must succeed with an empty result, not an error");
    assert!(hits.is_empty());
}

// ---------- 本体テスト: SearchTimeFilter ----------

// 対象ビヘイビア: RLS-1。`PrefilterIndex` と同じマトリクス（可視率 × テナント視点 × k）を
// `SearchTimeFilter::search` に対して検証する（TASK-134 経路）。10 テナント均等割り当てに
// して低可視率 10% の経路も実際に生成する（[`private_rate_for_visible_rate`] 参照。
// codex-review 指摘対応）。
#[test]
fn rls1_search_time_filter_no_violations_across_rate_tenant_k_matrix() {
    const NUM_ROWS: u64 = 3_000;
    const TENANT_COUNT: usize = RATE_MATRIX_TENANT_NAMES.len();

    for (rate_idx, &visible_rate) in [0.9, 0.5, 0.1].iter().enumerate() {
        let private_rate = private_rate_for_visible_rate(visible_rate, TENANT_COUNT);
        let tenants: Vec<(&str, f64)> = RATE_MATRIX_TENANT_NAMES
            .iter()
            .map(|&name| (name, private_rate))
            .collect();

        let path = unique_db_path(&format!("rls1-stf-{rate_idx}"));
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let seed = 6000 + rate_idx as u64;
        let truth = seed_multi_tenant_corpus(&storage, NUM_ROWS, &tenants, seed);

        let filter = SearchTimeFilter::build(&storage, TABLE).expect("build filter");

        for &(viewer, _) in &tenants[..2] {
            let ctx =
                PolicyContext::with_visibilities(viewer, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant");
            let allowed = allowed_ids(&truth, viewer, true);
            assert_visible_ratio_close_to_target(
                allowed.len(),
                NUM_ROWS,
                visible_rate,
                viewer,
                rate_idx,
            );

            assert_eq!(
                filter.len(&ctx).expect("len ok"),
                allowed.len(),
                "search-time filter visible-row count must match the independent oracle \
                 (viewer={viewer}, rate_idx={rate_idx})"
            );

            for &k in &[5usize, allowed.len() + 50] {
                for query_idx in 0..3u64 {
                    let query = random_query(9500 + rate_idx as u64 * 100 + query_idx);
                    let hits = filter.search(&ctx, &query, k).expect("search ok");
                    let violations = count_policy_violations(&hits, &allowed, k);
                    assert_eq!(
                        violations, 0,
                        "SearchTimeFilter leaked disallowed rows (viewer={viewer}, k={k}, \
                         rate_idx={rate_idx}, query={query_idx})"
                    );
                    assert_result_count_matches_visible_ceiling(&hits, allowed.len(), k);
                }
            }
        }
    }
}

// 対象ビヘイビア: RLS-1。可視行 0 件テナント（[`rls1_prefilter_zero_visible_tenant_returns_empty_without_error`]
// と同じ構成）を `SearchTimeFilter` に対しても検証する。
#[test]
fn rls1_search_time_filter_zero_visible_tenant_returns_empty_without_error() {
    const NUM_ROWS: u64 = 500;
    let tenants: [(&str, f64); 2] = [("tenant-a", 1.0), ("tenant-b", 1.0)];

    let path = unique_db_path("rls1-stf-zero");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_multi_tenant_corpus(&storage, NUM_ROWS, &tenants, 6500);

    let filter = SearchTimeFilter::build(&storage, TABLE).expect("build filter");
    let ctx_zero =
        PolicyContext::with_visibilities("zero-tenant", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    assert_eq!(filter.len(&ctx_zero).expect("len ok"), 0);
    assert!(filter.is_empty(&ctx_zero).expect("is_empty ok"));

    let hits = filter
        .search(&ctx_zero, &random_query(6501), 20)
        .expect("search must succeed with an empty result, not an error");
    assert!(hits.is_empty());
}

// 対象ビヘイビア: RLS-1。同一 `SearchTimeFilter` インスタンスに異なる `PolicyContext` を
// 連続適用し、直前のコンテキストの許可集合が次の検索結果へ漏れないことを検証する
// （TASK-134: ポリシーを構築時に束縛しない経路特有の回帰）。
#[test]
fn rls1_search_time_filter_does_not_leak_across_consecutive_policy_switches() {
    const NUM_ROWS: u64 = 1_500;
    // private_rate=1.0: 全行を Private にする。Public 行は両テナントから見えるため
    // （TABLE-9 のポインタ表記対象）、id 集合の素性を検証するには全行を Private にして
    // テナント固有の可視性にする必要がある。
    let tenants: [(&str, f64); 2] = [("tenant-a", 1.0), ("tenant-b", 1.0)];

    let path = unique_db_path("rls1-stf-switch");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let truth = seed_multi_tenant_corpus(&storage, NUM_ROWS, &tenants, 7000);

    let filter = SearchTimeFilter::build(&storage, TABLE).expect("build filter");

    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let allowed_a = allowed_ids(&truth, "tenant-a", true);
    let allowed_b = allowed_ids(&truth, "tenant-b", true);

    // 交互に ctx_a / ctx_b を適用し、毎回そのコンテキストの許可集合のみで判定する。
    for round in 0..5u64 {
        let query = random_query(8500 + round);

        let hits_a = filter.search(&ctx_a, &query, 20).expect("search ctx_a ok");
        assert_eq!(
            count_policy_violations(&hits_a, &allowed_a, 20),
            0,
            "ctx_a search must not leak tenant-b rows after a prior ctx_b call (round={round})"
        );

        let hits_b = filter.search(&ctx_b, &query, 20).expect("search ctx_b ok");
        assert_eq!(
            count_policy_violations(&hits_b, &allowed_b, 20),
            0,
            "ctx_b search must not leak tenant-a rows after a prior ctx_a call (round={round})"
        );

        // 両者は互いに素な許可集合を持つため（全行 Private・2 テナント構成）、
        // 返却 id 集合も必ず交差しない。
        let ids_a: BTreeSet<u64> = hits_a.iter().map(|h| h.id).collect();
        let ids_b: BTreeSet<u64> = hits_b.iter().map(|h| h.id).collect();
        assert!(
            ids_a.is_disjoint(&ids_b),
            "consecutive policy switches must not mix result id sets (round={round})"
        );
    }
}
