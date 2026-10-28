//! `engine::rls::RlsSafetyNet`（TASK-136・RLS-5。ポインタ: `docs/spec/05-tasks.md`
//! TASK-136・`docs/spec/04-behavior/rls.md` RLS-5・`docs/spec/04-behavior/sql-surface.md`
//! SQL-7）の結合テスト。`tests/rls_security.rs`（TASK-135）と同じ流儀
//! （`unique_db_path` + `CleanupGuard`、xorshift64* 決定的乱数、オラクルは production の
//! `PolicyContext::is_visible` を一切呼ばずシード時に独立に記録する）を踏襲する。
//!
//! `tests/sql_evaluation_order.rs`（TASK-76）は `execute_sql` 経由で `HINT ORDER` 全 6
//! 順列 × `WHERE visible()` の有無 × 複数テナント視点の不許可行混入 0 件を既に検証
//! しているが、`execute_sql` の候補集合は常に事前フィルタ済み（`arena`）を経由するため、
//! 安全網（`RlsSafetyNet`）単体が実際に不可視行を落とすことの証明にはならない
//! （事前フィルタが唯一の実効的な防御線であることは `rls.rs`・`sql/plan.rs` の
//! モジュールドキュメント参照）。本ファイルは [`VectorArena::build`]（無フィルタ。
//! 事前フィルタを経由しない候補集合構築の模擬）で全テナント行を含む arena を作り、
//! `RlsSafetyNet::apply` を直接適用することで、安全網単体が defense-in-depth として
//! 独立に機能することを機械検証する。

use std::collections::{BTreeSet, HashMap};

use engine::arena::VectorArena;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::rls::RlsSafetyNet;
use engine::storage::{RowInput, Storage, Visibility};

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した（`tests/rls_security.rs`・
// `tests/rls_prefilter.rs` と同じ流儀。Bugbot 指摘: ローカル再定義は `process_salt`
// と cleanup リトライを欠くため、共通モジュールに揃える）。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// テナント単位へ分割してシード投入する共通ヘルパ（複数テストへの複製を避けるため
// `src/test_util/seed_rows.rs` へ一本化した。`temp_db.rs` と同じ取り込み方式）。
use crate::seed_rows;
use seed_rows::seed_rows_grouped_by_tenant;

// ---------- 決定的擬似乱数（xorshift64*。`tests/rls_security.rs` と同一実装） ----------

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

    fn choose<'a, T>(&mut self, choices: &'a [T]) -> &'a T {
        let idx = (self.next_u64() as usize) % choices.len();
        &choices[idx]
    }
}

// ---------- テスト共通のセットアップ ----------

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

const DIM: u32 = 8;
const TABLE: &str = "docs";

fn schema_for(table_name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        table_name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
    )
}

/// シード時に記録する 1 行分の真値（production の `is_visible` を経由せず、テスト側で
/// 独立に保持する）。
#[derive(Clone, Copy)]
struct RowTruth {
    tenant: &'static str,
    visibility: Visibility,
}

/// [`PolicyContext::is_visible`] を呼ばずに、テスト側で独立に可視性を判定するオラクル
/// （`tests/rls_security.rs` と同一契約）。
fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => allow_private && row.tenant == viewer_tenant,
    }
}

/// 複数テナントにまたがる決定的コーパスを構築し、行ごとの真値を id → [`RowTruth`] で
/// 返す（`tests/rls_security.rs::seed_multi_tenant_corpus` と同方式）。
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

// ---------- §1: 安全網単体の独立検証（事前フィルタ迂回の模擬） ----------

/// `VectorArena::build`（無フィルタ。事前フィルタを経由しない候補集合構築の模擬）で
/// 全テナント行を含む arena を作り、`CpuScalarProvider` で Top-k を取得してから
/// `RlsSafetyNet::apply` に通す。可視率 90%→10% × 複数テナント視点（`allow_private`
/// あり／なし） × 複数 k を横断し、(a) 不許可 id が結果に 0 件、(b) `dropped()` が
/// 実際の不許可ヒット数と一致、(c) 許可行の相対順序が保たれることを検証する。
#[test]
fn safety_net_independently_removes_disallowed_hits_from_unfiltered_arena() {
    let path = unique_db_path("independent-verification");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);

    // 可視率 90%→10% を横断するテナント構成（tenant-0 を viewer、他を「他テナント」
    // として混在させる。private_rate は各テナントで変え、混入余地を作る）。
    let tenants: Vec<(&'static str, f64)> = vec![
        ("tenant-0", 0.10),
        ("tenant-1", 0.30),
        ("tenant-2", 0.50),
        ("tenant-3", 0.70),
        ("tenant-4", 0.90),
    ];
    let truth = seed_multi_tenant_corpus(&storage, 400, &tenants, 0xC0FF_EE00_1234_5678);

    // 事前フィルタを経由しない候補集合構築の模擬（TASK-136 モジュールドキュメント
    // 「候補集合の構築元が将来広がった場合」の想定シナリオ）。
    let arena = VectorArena::build(&storage, TABLE).expect("build unfiltered arena");
    let arena_index_by_id: HashMap<u64, usize> = arena
        .ids()
        .iter()
        .enumerate()
        .map(|(idx, id)| (*id, idx))
        .collect();
    let provider = CpuScalarProvider;

    let viewer_tenants = ["tenant-0", "tenant-2", "tenant-4"];
    let ks = [1usize, 10, 50, 200];

    for (viewer_idx, &viewer) in viewer_tenants.iter().enumerate() {
        for allow_private in [false, true] {
            let ctx = if allow_private {
                PolicyContext::with_visibilities(viewer, [Visibility::Public, Visibility::Private])
            } else {
                PolicyContext::new(viewer)
            }
            .expect("valid tenant");
            let expected_allowed = allowed_ids(&truth, viewer, allow_private);

            for &k in &ks {
                // viewer 文字列長は tenant-0/tenant-2/tenant-4 で等しく（いずれも 8
                // バイト）シードとして使えないため、ループ内の viewer_idx（0..2）を
                // 使い viewer ごとに異なるクエリを生成する。
                let query = random_query(k as u64 ^ (viewer_idx as u64) << 8);
                let input = SearchInput {
                    ids: arena.ids(),
                    vectors: arena.vectors(),
                    dim: arena.dim(),
                    query: &query,
                    k,
                };
                let raw_hits = provider.search(input).expect("provider search");
                let hits: Vec<(u64, f64)> =
                    raw_hits.iter().map(|h| (h.id, h.score as f64)).collect();

                // 期待される除去件数（このヒット集合のうち不許可 id の数）を、安全網とは
                // 独立に、シード時の真値のみから算出する。
                let expected_dropped = hits
                    .iter()
                    .filter(|(id, _)| !expected_allowed.contains(id))
                    .count();

                let net = RlsSafetyNet::new(&ctx);
                let verified = net.apply(hits.clone(), |id| {
                    let index = *arena_index_by_id.get(&id)?;
                    let tenant = arena.tenant_id(index)?;
                    let visibility = arena.visibility(index)?;
                    Some((tenant, visibility))
                });

                for &(id, _) in verified.hits() {
                    assert!(
                        expected_allowed.contains(&id),
                        "disallowed id leaked: id={id} viewer={viewer} allow_private={allow_private} k={k}"
                    );
                }
                assert_eq!(
                    verified.dropped(),
                    expected_dropped,
                    "dropped() must equal disallowed hit count: viewer={viewer} allow_private={allow_private} k={k}"
                );

                // 相対順序保持: 生き残った id 列は元 hits の id 列の部分列であること。
                let original_ids: Vec<u64> = hits.iter().map(|(id, _)| *id).collect();
                let surviving_ids: Vec<u64> = verified.hits().iter().map(|(id, _)| *id).collect();
                let mut cursor = 0usize;
                for id in &surviving_ids {
                    let found = original_ids[cursor..]
                        .iter()
                        .position(|oid| oid == id)
                        .unwrap_or_else(|| {
                            panic!("surviving id {id} not found in original order from cursor {cursor}")
                        });
                    cursor += found + 1;
                }
            }
        }
    }
}

// ---------- §2: `execute_sql` 経由の順序保持（RLS 段を最後尾に置いても並べ替えない） ----------

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// `WHERE visible()` のみのクエリで、既定順序（`RLS, SCALAR, DISTANCE`）と
/// `HINT ORDER(DISTANCE, SCALAR, RLS)`（RLS 段を最後尾）の結果 id 列が一致すること
/// を確認する。全行が単一テナント・`Public` で `WHERE visible()` の事前フィルタを
/// 素通りするため、本テストは安全網（`RlsSafetyNet::apply`）が実際に行を drop する
/// ケースでの順序保持は検証しない（それは HINT ORDER 自体の実行結果が RLS 段の
/// 位置に依存せず安定していることの検証）。安全網が drop を伴っても部分列として
/// 順序を保つことは §1（`RlsSafetyNet::apply` を直接駆動する
/// `safety_net_independently_removes_disallowed_hits_from_unfiltered_arena`）の
/// subsequence 検証でカバー済み。
#[test]
fn execute_sql_hint_order_rls_last_matches_default_order() {
    let path = unique_db_path("order-preserved-rls-last");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema_for(TABLE, 2))
        .expect("create table");
    let rows: [(u64, [f32; 2]); 6] = [
        (1, [1.0, 0.0]),
        (2, [0.9, 0.1]),
        (3, [0.8, 0.2]),
        (4, [0.7, 0.3]),
        (5, [0.6, 0.4]),
        (6, [0.5, 0.5]),
    ];
    for (id, emb) in rows {
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
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[engine::row_codec::Value::Vector(emb.to_vec())],
            &op_id,
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let default_result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE visible() ORDER BY embedding <=> '[1.0,0.0]' LIMIT 6",
        )
        .expect("default order execution should succeed");
    let rls_last_result = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE visible() ORDER BY embedding <=> '[1.0,0.0]' LIMIT 6 HINT ORDER(DISTANCE, SCALAR, RLS)",
        )
        .expect("RLS-last order execution should succeed");

    let default_ids: Vec<u64> = default_result.rows.iter().map(|r| r.id).collect();
    let rls_last_ids: Vec<u64> = rls_last_result.rows.iter().map(|r| r.id).collect();
    assert_eq!(
        default_ids, rls_last_ids,
        "HINT ORDER with RLS stage last must match default stage order"
    );
}
