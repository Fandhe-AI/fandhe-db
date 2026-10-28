//! `engine::tenant`（TASK-95・対象ビヘイビア: RECOVER-4）の機械検証。
//!
//! テナント境界を越える読み取り 10 回・書き込み（INSERT/UPDATE/DELETE 各 10 回。計
//! 40 試行）の遮断が `wire_code`（`42501`／`P0002`）で明示的に固定され、対象データが
//! 試行前後で不変（構造同値に加え、行数＋内容ハッシュの一致でも二重に検証）である
//! ことを検証する。
//!
//! `tests/tenant_isolation.rs`（TASK-89）・`tests/rls_security.rs`（TASK-133）の
//! シード手法（決定的 xorshift64*・`unique_db_path` + `CleanupGuard`）を踏襲し、
//! テスト側だけが持つグラウンドトゥルースを独立オラクルとして使う
//! （`PolicyContext::is_visible`/`is_owner` の実装バグからも独立させるため、本体の
//! 判定 API はオラクル側で再利用しない）。
//!
//! 本ファイルが担わない範囲: TABLE-12・RLS-9（同一 id 名前空間化・応答同一性）の
//! 検証は `tests/row_id_tenant_scope.rs` が担う。wire プロトコル経由の応答同一性・
//! レイテンシ分布比較は兄弟 Issue（#737・#738）の担当。SQL 表層に `UPDATE`/`DELETE`
//! 構文が存在しないため、SQL 経由の越境 UPDATE/DELETE は構造的に対象外
//! （攻撃面は Rust API `RowInput::tenant_id` ≠ ctx に限られる）。

use std::collections::BTreeMap;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::rls::{PrefilterIndex, SearchTimeFilter};
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant::{self, TenantWriteError};

// ---------- 決定的擬似乱数（xorshift64*。他の tenant/rls 結合テストと同一実装） ----------

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
}

// ---------- テスト共通のセットアップ ----------

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した（Bugbot Low 指摘・PR #194:
// このファイルにローカル複製が残っていた）。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 8;
const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
    )
}

/// 攻撃側（tenant-a）が保持しうる最も広い許可可視性（`Public`・`Private` の両方）を持つ
/// `PolicyContext`。読み取り側検証では「最も広い許可でも越境できないこと」を示すために
/// 使う。書き込み側の認可（`is_owner`）は可視性ラベルを見ないため、この ctx でも
/// tenant-b 名義の書き込みは一貫して拒否される想定。
fn attacker_ctx() -> PolicyContext {
    PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn victim_ctx() -> PolicyContext {
    PolicyContext::new(TENANT_B).expect("valid tenant")
}

/// テーブル全行を `scan_table_page` でカーソルが尽きるまで巡回して収集する、実装非依存の
/// チェッカー。キーは行ストアの物理キーと同じ `(tenant_id, id)`（対象ビヘイビア: TABLE-12。
/// 行 `id` の一意性スコープがテナント内のため、`id` 単独をキーにすると異なるテナントの
/// 同一 `id` の行が畳み込まれ、越境書き込みの検出が甘くなる）。値は
/// `(visibility, embedding, metadata)`。
/// スナップショットの値: `(is_public, embedding, metadata)`。
type RowSnapshotValue = (bool, Vec<f32>, Vec<u8>);
/// スナップショット全体: キーは物理キーと同形の `(tenant_id, id)`（TABLE-12）。
type TableSnapshot = BTreeMap<(String, u64), RowSnapshotValue>;

// ---------- RECOVER-4 の試行数タリー ----------
//
// 対象ビヘイビア: RECOVER-4「越境読み取り 10 回・書き込み 30 回（INSERT/UPDATE/DELETE
// 各 10 回）の遮断」。読み取り側は `PrefilterIndex`/`SearchTimeFilter` 5 回ずつの
// フェーズ 1（`recover4_read_breach_attempts_never_return_foreign_private_rows` の
// `attempts` カウンタ）で固定する。書き込み側は 3 種別 ×10 回を合算した総数を固定する
// （個別の種別内訳は各テストの試行ループが担保する）。
const READ_ATTEMPTS: u32 = 10;
const WRITE_ATTEMPTS_PER_KIND: usize = 10;
const WRITE_ATTEMPTS: usize = WRITE_ATTEMPTS_PER_KIND * 3;
#[allow(dead_code)] // 受入基準の「読み取り 10 回・書き込み 30 回」計 40 回を文書化する定数。
const TOTAL_ATTEMPTS: usize = READ_ATTEMPTS as usize + WRITE_ATTEMPTS;

/// テスト専任の差分検出用ハッシュ（FNV-1a 64bit。`tests/hnsw.rs` の
/// `graph_fingerprint_is_stable_across_representation_change` と同一定数）。
/// `engine::recovery::content_hash`（自作 SHA-256）は `pub(crate)` で結合テストから
/// 到達不能なため、本ファイル専用の非暗号用途ハッシュとして別途実装する
/// （セキュリティ用途ではなく、スナップショット比較の取りこぼしを防ぐための
/// 補助的な差分検出のみに使う）。
const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

fn fnv1a_update(hash: u64, bytes: &[u8]) -> u64 {
    let mut h = hash;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// `TableSnapshot`（`BTreeMap` により `(tenant_id, id)` 昇順で決定的に反復される）を
/// 長さ前置つきの正規化バイト列へ畳み込み、行数と合わせたフィンガープリントを返す。
/// 境界の曖昧さ（可変長フィールドの連結による衝突）を長さ前置で排除する。
fn snapshot_fingerprint(snapshot: &TableSnapshot) -> (usize, u64) {
    let mut hash = FNV_OFFSET_BASIS;
    hash = fnv1a_update(hash, &(snapshot.len() as u64).to_le_bytes());
    for ((tenant_id, id), (is_public, embedding, metadata)) in snapshot {
        let tenant_bytes = tenant_id.as_bytes();
        hash = fnv1a_update(hash, &(tenant_bytes.len() as u64).to_le_bytes());
        hash = fnv1a_update(hash, tenant_bytes);
        hash = fnv1a_update(hash, &id.to_le_bytes());
        hash = fnv1a_update(hash, &[u8::from(*is_public)]);
        hash = fnv1a_update(hash, &(embedding.len() as u64).to_le_bytes());
        for v in embedding {
            hash = fnv1a_update(hash, &v.to_bits().to_le_bytes());
        }
        hash = fnv1a_update(hash, &(metadata.len() as u64).to_le_bytes());
        hash = fnv1a_update(hash, metadata);
    }
    (snapshot.len(), hash)
}

/// RECOVER-4「対象データが試行前後で不変」を、構造同値（`BTreeMap ==`）に加えて
/// 行数＋内容ハッシュの一致でも固定する（`BTreeMap ==` だけでは読み落としうる
/// フィールド単位の取りこぼしを、独立した畳み込み方式のハッシュで多重に検出する）。
fn assert_snapshot_unchanged(before: &TableSnapshot, after: &TableSnapshot, what: &str) {
    assert_eq!(before, after, "{what}: table contents must be unchanged");
    assert_eq!(
        snapshot_fingerprint(before),
        snapshot_fingerprint(after),
        "{what}: (row_count, content_hash) must be unchanged"
    );
}

fn snapshot_table(storage: &Storage) -> TableSnapshot {
    let mut out = BTreeMap::new();
    // カーソルも物理キーと同形。
    let mut after: Option<(String, u64)> = None;
    loop {
        let (page, next) = storage
            .scan_table_page(
                TABLE,
                after.as_ref().map(|(t, id)| (t.as_str(), *id)),
                10_000,
            )
            .expect("scan_table_page ok");
        if page.is_empty() && next.is_none() {
            break;
        }
        for row in page {
            out.insert(
                (row.tenant_id.clone(), row.id),
                (
                    matches!(row.visibility, Visibility::Public),
                    row.embedding.clone(),
                    row.metadata.clone(),
                ),
            );
        }
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    out
}

/// 決定的な合成コーパスを構築する: tenant-a・tenant-b それぞれ `rows_per_tenant` 行、
/// `Public`/`Private` 混在。id は 1..=2*rows_per_tenant を tenant-a → tenant-b の順に
/// 割り当てる。
fn seed_corpus(storage: &Storage, rows_per_tenant: u64, seed: u64) {
    storage.create_table(&schema()).expect("create table");
    let mut rng = Xorshift64::new(seed);
    let tenants = [TENANT_A, TENANT_B];
    let mut rows: Vec<(u64, Vec<f32>, &'static str, Visibility)> = Vec::new();
    let mut id = 1u64;
    for tenant in tenants {
        for _ in 0..rows_per_tenant {
            let visibility = if rng.next_f64() < 0.5 {
                Visibility::Public
            } else {
                Visibility::Private
            };
            let embedding: Vec<f32> = (0..DIM).map(|_| rng.next_f32_signed()).collect();
            rows.push((id, embedding, tenant, visibility));
            id += 1;
        }
    }
    // 投入はテナント境界付きバッチ API（`tenant::insert_rows`）経由で行う
    // （codex-review P0 指摘・PR #194 対応で `Storage::insert_rows_into_table` は
    // `pub(crate)` 化した）。ガード付き API は 1 バッチ内のテナント混在を
    // `Forbidden` で拒否するため、テナントごとにバッチを分ける。
    for tenant in tenants {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let inputs: Vec<(u64, RowInput<'_>)> = rows
            .iter()
            .filter(|(_, _, row_tenant, _)| *row_tenant == tenant)
            .map(|(id, emb, row_tenant, vis)| {
                (
                    *id,
                    RowInput {
                        tenant_id: row_tenant,
                        visibility: *vis,
                        embedding: emb,
                        metadata: &[],
                    },
                )
            })
            .collect();
        tenant::insert_rows(
            storage,
            TABLE,
            &ctx,
            &inputs,
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("seed corpus batch insert");
    }
}

// 対象ビヘイビア: RECOVER-4。テナント境界を越える書き込み（INSERT/UPDATE/DELETE）試行が
// 全件拒否され、対象データが試行前後で不変であることを検証する。
#[test]
fn recover4_write_breach_attempts_are_all_rejected_and_data_is_unchanged() {
    let path = unique_db_path("write-breach");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_corpus(&storage, 20, 1000);

    let before = snapshot_table(&storage);
    let attacker = attacker_ctx();

    // tenant-b の Public 行と Private 行の両方を標的に含める。
    // スナップショットのキーは `(tenant_id, id)`（TABLE-12）。
    let victim_ids: Vec<u64> = before
        .keys()
        .filter(|(tenant, _)| tenant == TENANT_B)
        .map(|(_, id)| *id)
        .collect();
    assert!(victim_ids.len() >= 20, "seed must contain tenant-b rows");
    let victim_public_id = before
        .iter()
        .find(|((tenant, _), (is_public, ..))| tenant == TENANT_B && *is_public)
        .map(|((_, id), _)| *id)
        .expect("seed must contain a tenant-b public row");
    let victim_private_id = before
        .iter()
        .find(|((tenant, _), (is_public, ..))| tenant == TENANT_B && !*is_public)
        .map(|((_, id), _)| *id)
        .expect("seed must contain a tenant-b private row");

    let mut results: Vec<Result<(), TenantWriteError>> = Vec::new();

    // INSERT 10 回: いずれも「他テナント（tenant-b）名義の行を書き込む」越境試行
    // （Forbidden 期待）。偶数回は未使用 id、奇数回は tenant-b の既存 id を対象にし、
    // 対象 id の使用状況にかかわらず同じ拒否になることも併せて確認する。
    //
    // 注意（TABLE-12・RLS-9）: 「自テナント名義で他テナントの既存 id へ INSERT する」
    // 操作はもはや越境ではなく、自テナントの名前空間への正当な挿入として成功する
    // （物理キーが `(tenant_id, id)` で名前空間化されたため、他テナント行は上書き
    // されない）。その契約の検証は `tests/row_id_tenant_scope.rs` が担う。
    for i in 0..10u64 {
        let target = if i % 2 == 0 {
            100_000 + i
        } else {
            victim_ids[(i as usize) % victim_ids.len()]
        };
        let row = RowInput {
            tenant_id: TENANT_B,
            visibility: Visibility::Public,
            embedding: &[1.0; DIM as usize],
            metadata: &[],
        };
        let r = tenant::insert_row(
            &storage,
            TABLE,
            &attacker,
            target,
            &row,
            // `seed_corpus` が tenant-a 名義の正当な投入で "test-op" を既に使用済み
            // （TASK-94・RECOVER-3 は operation_id をテナント×テーブル単位で一意化する）
            // ため、越境試行の検証には別の未使用 operation_id を使う（PR #247
            // codex-review 指摘対応。越境試行はいずれも所有権判定より先に拒否・abort
            // されるため台帳へは残らないが、"test-op" を再利用すると seed 時点の
            // 正当な記録と衝突し `DuplicateOperationId` を誤って観測してしまう）。
            &engine::recovery::required_op_id::OperationId::parse("test-op-breach")
                .expect("valid operation_id"),
        );
        assert!(matches!(r, Err(TenantWriteError::Forbidden)));
        // RECOVER-4・ERR-2: 越境 INSERT はテナント不一致（クライアント自身の入力に
        // 起因し存在情報を含まない）として一律 `42501` へ写像される（`Forbidden` の
        // variant 固定に加え、応答契約そのものである `wire_code` も明示的に固定する）。
        assert_eq!(r.as_ref().unwrap_err().wire_code(), "42501");
        results.push(r);
    }

    // UPDATE 10 回: 偶数回は tenant-b の行（Public/Private 交互）を tenant-a ctx で
    // 更新（NotFound 期待）。奇数回は tenant-a 自身の行を tenant-b へ付け替える試行
    // （Forbidden 期待）。
    let attacker_own_ids: Vec<u64> = before
        .keys()
        .filter(|(tenant, _)| tenant == TENANT_A)
        .map(|(_, id)| *id)
        .collect();
    for i in 0..10u64 {
        if i % 2 == 0 {
            let target = if i % 4 == 0 {
                victim_public_id
            } else {
                victim_private_id
            };
            let row = RowInput {
                tenant_id: TENANT_A,
                visibility: Visibility::Public,
                embedding: &[2.0; DIM as usize],
                metadata: &[],
            };
            // TASK-101（RECOVER-10）: この分岐は `row.tenant_id == ctx` の所有権検査を
            // 通過するため台帳照合まで到達する。`target` ごとに内容ハッシュが変わる
            // ため、同一 operation_id を使い回すと 2 回目以降が
            // OperationIdContentMismatch になり、本テストが検証したい NotFound を
            // 独立に確認できなくなる。イテレーションごとに一意の operation_id を使う。
            let op_id = format!("test-op-update-{i}");
            let r = tenant::update_row(
                &storage,
                TABLE,
                &attacker,
                target,
                &row,
                &engine::recovery::required_op_id::OperationId::parse(&op_id)
                    .expect("valid operation_id"),
            );
            assert!(matches!(r, Err(TenantWriteError::NotFound)));
            // RECOVER-4・RLS-9: 存在／非所有を区別しない一律 `P0002` への写像
            // （受入基準の「不可視扱い」に相当。存在情報を漏らさない fail-closed 判定）。
            assert_eq!(r.as_ref().unwrap_err().wire_code(), "P0002");
            results.push(r);
        } else {
            let own_id = attacker_own_ids[(i as usize) % attacker_own_ids.len()];
            let row = RowInput {
                tenant_id: TENANT_B,
                visibility: Visibility::Public,
                embedding: &[2.0; DIM as usize],
                metadata: &[],
            };
            let r = tenant::update_row(
                &storage,
                TABLE,
                &attacker,
                own_id,
                &row,
                // "test-op-breach"（同上）。
                &engine::recovery::required_op_id::OperationId::parse("test-op-breach")
                    .expect("valid operation_id"),
            );
            assert!(matches!(r, Err(TenantWriteError::Forbidden)));
            assert_eq!(r.as_ref().unwrap_err().wire_code(), "42501");
            results.push(r);
        }
    }

    // DELETE 10 回: tenant-b の行（Public を必ず含む）を tenant-a ctx で削除試行
    // （NotFound 期待）。
    for i in 0..10u64 {
        let target = if i == 0 {
            victim_public_id
        } else {
            victim_ids[(i as usize) % victim_ids.len()]
        };
        // TASK-101（RECOVER-10）: delete は所有権検査を持たず台帳照合まで必ず到達する。
        // `target` ごとに内容ハッシュ（id のみ）が変わるため、同一 operation_id を
        // 使い回すと 2 回目以降が OperationIdContentMismatch になってしまう。
        let op_id = format!("test-op-delete-{i}");
        let r = tenant::delete_row(
            &storage,
            TABLE,
            &attacker,
            target,
            &engine::recovery::required_op_id::OperationId::parse(&op_id)
                .expect("valid operation_id"),
        );
        assert!(matches!(r, Err(TenantWriteError::NotFound)));
        assert_eq!(r.as_ref().unwrap_err().wire_code(), "P0002");
        results.push(r);
    }

    // 全 30 件（INSERT/UPDATE/DELETE 各 10 回）が Err であること（成功件数 0）。
    assert_eq!(results.len(), WRITE_ATTEMPTS);
    let rejected = results.iter().filter(|r| r.is_err()).count();
    assert_eq!(rejected, WRITE_ATTEMPTS);
    let succeeded = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(succeeded, 0);
    // 拒否はすべて RECOVER-4 が定める 2 分類（`42501`／`P0002`）のいずれかに落ちる。
    let rejected_by_wire_code = results
        .iter()
        .filter_map(|r| r.as_ref().err())
        .filter(|e| matches!(e.wire_code(), "42501" | "P0002"))
        .count();
    assert_eq!(rejected_by_wire_code, WRITE_ATTEMPTS);

    // エラーの Display/Debug 文字列に被害側テナント名・対象 id が含まれないこと
    // （security.md P0「存在情報を漏らさない」の回帰検証）。
    for r in &results {
        if let Err(e) = r {
            let display = format!("{e}");
            let debug = format!("{e:?}");
            assert!(
                !display.contains(TENANT_B),
                "Display leaked tenant: {display}"
            );
            assert!(!debug.contains(TENANT_B), "Debug leaked tenant: {debug}");
            assert!(
                !display.contains(&victim_public_id.to_string()),
                "Display leaked row id: {display}"
            );
            assert!(
                !debug.contains(&victim_public_id.to_string()),
                "Debug leaked row id: {debug}"
            );
        }
    }

    // 試行後、テーブル全体が試行前と完全一致すること（差分 0 件・行数＋内容ハッシュも
    // 不変）。
    let after = snapshot_table(&storage);
    assert_snapshot_unchanged(&before, &after, "in-process view after 30 breach attempts");

    // 永続イメージの不変性も確認する（RECOVER-4: 「対象データが試行前後で不変」は
    // プロセス内ビューだけでなくディスク上の状態を指す。ハンドルを閉じて再オープンし、
    // commit されなかった拒否試行がディスクにも一切反映されていないことを検証する）。
    drop(storage);
    let reopened = Storage::open(&path).expect("reopen storage after breach attempts");
    let after_reopen = snapshot_table(&reopened);
    assert_snapshot_unchanged(
        &before,
        &after_reopen,
        "persisted image after reopening the database",
    );
}

// 対象ビヘイビア: RECOVER-4。読み取り経路（PrefilterIndex/SearchTimeFilter/
// tenant::visible_rows/EngineCore::get_row/EngineCore::search/EngineCore::execute_sql）を
// 巡回し、他テナントの Private 行が一切返らないことを検証する。
#[test]
fn recover4_read_breach_attempts_never_return_foreign_private_rows() {
    let path = unique_db_path("read-breach");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // rows_per_tenant=40（期待値約 20 件/テナント/可視性）で、後段の 10 標的サンプリング
    // （RECOVER-4 の「読み取り 10 回」）に十分な tenant-b Private 行数を確保する。
    seed_corpus(&storage, 40, 2000);

    let before = snapshot_table(&storage);
    let attacker = attacker_ctx();
    // RECOVER-4 の「読み取り 10 回」に対応する 10 個の標的（tenant-b の Private 行）。
    let victim_ids: Vec<u64> = before
        .iter()
        .filter(|((tenant, _), (is_public, ..))| tenant == TENANT_B && !*is_public)
        .map(|((_, id), _)| *id)
        .take(10)
        .collect();
    assert!(
        victim_ids.len() >= 10,
        "seed must contain at least 10 tenant-b private rows, got {}",
        victim_ids.len()
    );

    // 攻撃側が「見てよい」集合（自テナントの全行 + 他テナントの Public 行）。
    let allowed_ids: std::collections::HashSet<u64> = before
        .iter()
        .filter(|((tenant, _), (is_public, ..))| tenant == TENANT_A || *is_public)
        .map(|((_, id), _)| *id)
        .collect();
    for id in &victim_ids {
        assert!(
            !allowed_ids.contains(id),
            "sanity: victim private row {id} must not be in the allowed set"
        );
    }

    let mut exposures = 0u32;
    let mut attempts = 0u32;

    // フェーズ 1: `PrefilterIndex`/`SearchTimeFilter`（`&Storage` を借用する経路）。
    // 10 標的のうち偶数インデックスを `PrefilterIndex::search`、奇数インデックスを
    // `SearchTimeFilter::search` に割り当てる（計 10 試行の一部）。クエリベクトルには
    // 標的自身の埋め込みをそのまま使い、最近傍として最も露出しやすい条件にする。
    {
        let prefilter =
            PrefilterIndex::build(&storage, TABLE, &attacker).expect("build prefilter index");
        let search_time =
            SearchTimeFilter::build(&storage, TABLE).expect("build search-time filter");

        for (i, victim_id) in victim_ids.iter().enumerate() {
            // 値は `(is_public, embedding, metadata)`、キーは `(tenant_id, id)`（TABLE-12）。
            let query = &before[&(TENANT_B.to_string(), *victim_id)].1;
            let hits = if i % 2 == 0 {
                prefilter
                    .search(&attacker, &CpuScalarProvider, query, 5)
                    .expect("prefilter search ok")
            } else {
                search_time
                    .search(&attacker, query, 5)
                    .expect("search-time filter search ok")
            };
            attempts += 1;
            for h in &hits {
                assert!(allowed_ids.contains(&h.id));
                if h.id == *victim_id {
                    exposures += 1;
                }
            }
        }

        // `PrefilterIndex` は構築時 ctx と検索時 ctx の完全一致を要求する
        // （TASK-133・[`engine::rls::RlsError::ContextMismatch`]）。victim ctx で構築した
        // インデックスを attacker ctx で検索する試行は、越境の前にこの整合性検査で
        // fail-closed に拒否される（上記の 10 試行とは別の、越境試行としての追加検証）。
        let victim = victim_ctx();
        let victim_index =
            PrefilterIndex::build(&storage, TABLE, &victim).expect("build prefilter index");
        let mismatched = victim_index.search(
            &attacker,
            &CpuScalarProvider,
            &before[&(TENANT_B.to_string(), victim_ids[0])].1,
            5,
        );
        assert!(matches!(
            mismatched,
            Err(engine::rls::RlsError::ContextMismatch)
        ));

        // `tenant::visible_rows` は個別の検索試行としては数えないが、可視集合そのものに
        // 10 標的が一切含まれないことを別途確認する（読み取り経路の網羅）。
        let visible = tenant::visible_rows(&storage, TABLE, &attacker).expect("visible_rows ok");
        for victim_id in &victim_ids {
            assert!(!visible.iter().any(|r| r.id == *victim_id));
        }
        assert!(visible.iter().all(|r| allowed_ids.contains(&r.id)));
    }
    assert_eq!(
        attempts, READ_ATTEMPTS,
        "phase 1 must account for exactly {READ_ATTEMPTS} attempts"
    );

    // RECOVER-4「対象データが試行前後で不変」: フェーズ 1（読み取りのみ）の試行後、
    // テーブル全体が不変であることを確認する（読み取り経路は書き込みトランザクション
    // を一切開始しないため、書き込み側テストと異なり `&Storage` 借用中のまま検証できる）。
    let after_phase1 = snapshot_table(&storage);
    assert_snapshot_unchanged(&before, &after_phase1, "after phase 1 read attempts");

    // フェーズ 2: `EngineCore`（`Storage` の所有権を取る経路）。`get_row`・`search`・
    // `execute_sql` を巡回し、フェーズ 1 と別の 10 標的（tenant-b Public 行も含めて
    // 網羅する目的で同じ 10 件を再利用）に対して全経路が拒否・非露出であることを
    // 追加確認する（別集計・フェーズ 1 の 10 件を上書きしない）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    for victim_id in &victim_ids {
        // 点取得のキーは `(tenant_id, id)`（TABLE-12）。被害側テナントの行キーを
        // 直接指定しても、不可視のため `NotFound` に統一される（RLS-9）。
        let get_result = core.get_row(&attacker, TABLE, TENANT_B, *victim_id);
        assert!(matches!(get_result, Err(engine::core::CoreError::NotFound)));
        // エラーの Display/Debug 文字列に被害側テナント名・対象 id が含まれないこと
        // （security.md P0「存在情報を漏らさない」の回帰検証。書き込み側と同種の
        // 非漏えい確認を読み取り側の `CoreError` にも拡張する）。
        if let Err(e) = &get_result {
            let display = format!("{e}");
            let debug = format!("{e:?}");
            assert!(
                !display.contains(TENANT_B),
                "Display leaked tenant: {display}"
            );
            assert!(!debug.contains(TENANT_B), "Debug leaked tenant: {debug}");
            assert!(
                !display.contains(&victim_id.to_string()),
                "Display leaked row id: {display}"
            );
            assert!(
                !debug.contains(&victim_id.to_string()),
                "Debug leaked row id: {debug}"
            );
        }
    }

    let query = &before[&(TENANT_B.to_string(), victim_ids[0])].1;
    let hits = core
        .search(&attacker, TABLE, query, 5)
        .expect("core search ok");
    for h in &hits {
        assert!(allowed_ids.contains(&h.id));
        if victim_ids.contains(&h.id) {
            exposures += 1;
        }
    }

    // `execute_sql` はプレースホルダを持たないため、許可リスト経由のベクトルリテラルへ
    // 標的の埋め込みをそのまま埋め込む（SQL 表層は本タスクのスコープ外だが、読み取り
    // 経路の網羅のため最小限の SELECT を 1 件だけ通す）。
    let literal: String = query
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[{literal}]' LIMIT 5");
    let query_result = core.execute_sql(&attacker, &sql).expect("execute_sql ok");
    for row in &query_result.rows {
        assert!(allowed_ids.contains(&row.id));
        if victim_ids.contains(&row.id) {
            exposures += 1;
        }
    }

    assert_eq!(
        exposures, 0,
        "victim private row content must never be exposed to the attacker across any read path"
    );

    // RECOVER-4「対象データが試行前後で不変」（永続イメージ側）: `EngineCore` は
    // `Storage` の所有権を持つためプロセス内ビューを直接再取得できない。ハンドルを
    // 閉じてディスクから再オープンし、フェーズ 2 の読み取り試行（`get_row`・`search`・
    // `execute_sql`。いずれも読み取り専用でトランザクションを開始しない）がディスク上の
    // 状態に一切影響していないことを検証する。
    drop(core);
    let reopened = Storage::open(&path).expect("reopen storage after read breach attempts");
    let after_reopen = snapshot_table(&reopened);
    assert_snapshot_unchanged(
        &before,
        &after_reopen,
        "persisted image after all read breach attempts",
    );
}

// 対象ビヘイビア: RECOVER-4（正方向）。自テナント名義の書き込みは成功することを確認し、
// 拒否一辺倒の実装による誤 green を防ぐ。
#[test]
fn recover4_owner_writes_succeed_so_the_guard_is_not_vacuous() {
    let path = unique_db_path("owner-writes");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_corpus(&storage, 5, 3000);

    let owner =
        PolicyContext::with_visibilities(TENANT_A, [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    // 直接 `tenant::*` API 経由。
    let before = snapshot_table(&storage);
    let new_id = 900_001u64;
    let insert_row = RowInput {
        tenant_id: TENANT_A,
        visibility: Visibility::Public,
        embedding: &[0.5; DIM as usize],
        metadata: &[],
    };
    // TASK-101（RECOVER-10）: 台帳は (tenant, table, operation_id) 単位で内容ハッシュを
    // 持ち、insert/update/delete はそれぞれ異なる正規化入力を持つため、以下の
    // insert/update/delete へ同一 operation_id を使い回すと 2 回目以降が
    // OperationIdContentMismatch になる。操作ごとに別々の operation_id を使う。
    tenant::insert_row(
        &storage,
        TABLE,
        &owner,
        new_id,
        &insert_row,
        &engine::recovery::required_op_id::OperationId::parse("test-op-owner-insert")
            .expect("valid operation_id"),
    )
    .expect("owner insert ok");

    let own_id = before
        .keys()
        .find(|(t, _)| t == TENANT_A)
        .map(|(_, id)| *id)
        .expect("seed must contain a tenant-a row");
    let update_row_input = RowInput {
        tenant_id: TENANT_A,
        visibility: Visibility::Private,
        embedding: &[0.75; DIM as usize],
        metadata: &[],
    };
    tenant::update_row(
        &storage,
        TABLE,
        &owner,
        own_id,
        &update_row_input,
        &engine::recovery::required_op_id::OperationId::parse("test-op-owner-update")
            .expect("valid operation_id"),
    )
    .expect("owner update ok");

    let delete_target = before
        .keys()
        .filter(|(t, id)| t == TENANT_A && *id != own_id)
        .map(|(_, id)| *id)
        .next()
        .expect("seed must contain another tenant-a row");
    tenant::delete_row(
        &storage,
        TABLE,
        &owner,
        delete_target,
        &engine::recovery::required_op_id::OperationId::parse("test-op-owner-delete")
            .expect("valid operation_id"),
    )
    .expect("owner delete ok");

    // スナップショットのキーは `(tenant_id, id)`（TABLE-12）。
    let key = |id: u64| (TENANT_A.to_string(), id);
    let after = snapshot_table(&storage);
    assert!(after.contains_key(&key(new_id)));
    assert_eq!(after[&key(own_id)].2, update_row_input.metadata.to_vec());
    assert!(!after.contains_key(&key(delete_target)));

    // 期待した 3 件（insert 1・update 1・delete 1）以外に差分がないこと。
    let mut changed: Vec<u64> = Vec::new();
    let all_keys: std::collections::HashSet<(String, u64)> =
        before.keys().chain(after.keys()).cloned().collect();
    for k in all_keys {
        if before.get(&k) != after.get(&k) {
            changed.push(k.1);
        }
    }
    changed.sort_unstable();
    let mut expected = vec![new_id, own_id, delete_target];
    expected.sort_unstable();
    assert_eq!(changed, expected);

    // ハッシュ検査器の非 vacuous 化: 行数は insert 1・delete 1 で相殺され得る
    // （本テストでは +1 のまま）が、内容ハッシュは正当な 3 件の変更後に必ず変わる
    // ことを固定する。`assert_snapshot_unchanged` が差分を見逃さないことの検証。
    assert_ne!(
        snapshot_fingerprint(&before),
        snapshot_fingerprint(&after),
        "content hash must detect the 3 legitimate changes"
    );

    // `EngineCore` 委譲メソッド経由でも 1 件ずつ成功することを確認する。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let another_new_id = 900_002u64;
    let insert_row2 = RowInput {
        tenant_id: TENANT_A,
        visibility: Visibility::Public,
        embedding: &[0.25; DIM as usize],
        metadata: &[],
    };
    // TASK-101（RECOVER-10）: insert/update/delete それぞれ別の operation_id を使う
    // （上記と同じ理由）。
    let insert_op_id =
        OperationId::parse("op-engine-core-delegation-insert").expect("valid operation_id");
    let update_op_id =
        OperationId::parse("op-engine-core-delegation-update").expect("valid operation_id");
    let delete_op_id =
        OperationId::parse("op-engine-core-delegation-delete").expect("valid operation_id");
    core.insert_row(
        &owner,
        TABLE,
        another_new_id,
        &insert_row2,
        Some(&insert_op_id),
    )
    .expect("EngineCore::insert_row ok");
    let update_row2 = RowInput {
        tenant_id: TENANT_A,
        visibility: Visibility::Public,
        embedding: &[0.1; DIM as usize],
        metadata: &[],
    };
    core.update_row(
        &owner,
        TABLE,
        another_new_id,
        &update_row2,
        Some(&update_op_id),
    )
    .expect("EngineCore::update_row ok");
    core.delete_row(&owner, TABLE, another_new_id, Some(&delete_op_id))
        .expect("EngineCore::delete_row ok");
}

// 対象ビヘイビア: RECOVER-4（負方向・検査器の実効性）。ガードを経由しない生の書き込み
// （`Storage::insert_row_into_table`）で tenant-b 行を上書きした場合に `snapshot_table` の
// 前後比較が差分を検出できることの確認は、当該 API を `pub(crate)` 化した
// （codex-review P0 指摘・PR #194。クレート外部から到達不能にした）ことに伴い、
// クレート内ユニットテスト（`crates/engine/src/tenant.rs` の
// `raw_insert_row_into_table_bypasses_tenant_guard`）へ移設した。
