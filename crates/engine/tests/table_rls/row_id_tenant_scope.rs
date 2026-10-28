//! 行 `id` の一意性スコープ（テナント内）と書き込み経路の存在情報秘匿の機械検証
//! （対象ビヘイビア: TABLE-12・RLS-9。越境書き込み遮断そのものは RECOVER-4 で
//! `tests/tenant_breach.rs` が担当する。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-12・`rls.md` RLS-9・`recovery.md` RECOVER-4）。
//!
//! 検証対象は `engine::tenant` の書き込みガード（`insert_row`/`update_row`/`delete_row`）と、
//! その下で `(tenant_id, id)` に名前空間化された行ストア（`engine::catalog` の
//! テーブルスコープ行 API）。他テナントが同じ行 `id` を保持しているか否かで、
//! 応答（成否・`wire_code`・エラー文言）も後続の読み取り結果も変化しないことを、
//! 本体の判定 API に依存しないテスト側オラクル（期待値のベタ書き）で確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::{CpuScalarProvider, SearchHit};
use engine::policy::PolicyContext;
use engine::row_codec::Value as RowCodecValue;
use engine::sql::exec::Cell;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant::{self, TenantError, TenantWriteError};

// 一時 DB パス払い出しは共通ヘルパへ委譲する（Issue #173・Bugbot Low 指摘・PR #194）。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const DIM: u32 = 2;
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
const TENANT_C: &str = "tenant-c";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn row<'a>(tenant: &'a str, embedding: &'a [f32], metadata: &'a [u8]) -> RowInput<'a> {
    RowInput {
        tenant_id: tenant,
        visibility: Visibility::Public,
        embedding,
        metadata,
    }
}

fn open_seeded(label: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(label);
    let cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (storage, cleanup)
}

// (a) 対象ビヘイビア: TABLE-12 (b)。他テナントが保持する id と同じ id への
// 自テナント名義 INSERT は成功し、他テナントの行は一切変化しない
// （物理キーが `(tenant_id, id)` で名前空間化されているため上書きが起こらない）。
#[test]
fn table12_insert_with_an_id_held_by_another_tenant_succeeds_without_touching_that_row() {
    let (storage, _cleanup) = open_seeded("cross-tenant-id");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);

    tenant::insert_row(
        &storage,
        TABLE,
        &a,
        1,
        &row(TENANT_A, &[1.0, 0.0], b"a1"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-a insert id=1");
    tenant::insert_row(
        &storage,
        TABLE,
        &b,
        1,
        &row(TENANT_B, &[0.0, 1.0], b"b1"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-b must be able to use the same id in its own namespace");

    // 双方の行が独立に残っていること（テスト側で期待値をベタ書きし、本体の
    // 可視性判定 API には依存しない）。
    let a_row = storage
        .get_row_from_table(TABLE, TENANT_A, 1)
        .expect("tenant-a row must still exist");
    let b_row = storage
        .get_row_from_table(TABLE, TENANT_B, 1)
        .expect("tenant-b row must exist");
    assert_eq!(a_row.tenant_id, TENANT_A);
    assert_eq!(a_row.embedding, vec![1.0, 0.0]);
    assert_eq!(a_row.metadata, b"a1".to_vec());
    assert_eq!(b_row.tenant_id, TENANT_B);
    assert_eq!(b_row.embedding, vec![0.0, 1.0]);
    assert_eq!(b_row.metadata, b"b1".to_vec());
}

// (b) 対象ビヘイビア: TABLE-12 (a)。同一テナント内の重複 id は `23505` で拒否され、
// 既存行は上書きされない。
#[test]
fn table12_duplicate_id_within_the_same_tenant_is_rejected_with_23505() {
    let (storage, _cleanup) = open_seeded("same-tenant-dup");
    let a = ctx(TENANT_A);

    tenant::insert_row(
        &storage,
        TABLE,
        &a,
        7,
        &row(TENANT_A, &[1.0, 0.0], b"first"),
        &engine::recovery::required_op_id::OperationId::parse("test-op-1")
            .expect("valid operation_id"),
    )
    .expect("first insert ok");
    // 別の operation_id を使う（TASK-101・RECOVER-10）: 台帳照合は行書き込みより
    // 前に行われるため、ここで同一 operation_id を再利用すると
    // OperationIdContentMismatch（内容不一致）が先に検出されてしまい、本テストが
    // 検証したい「行 id レベルの衝突検出」を独立に確認できなくなる。
    let err = tenant::insert_row(
        &storage,
        TABLE,
        &a,
        7,
        &row(TENANT_A, &[0.0, 1.0], b"second"),
        &engine::recovery::required_op_id::OperationId::parse("test-op-2")
            .expect("valid operation_id"),
    )
    .expect_err("duplicate id within the same tenant must be rejected");
    assert!(matches!(err, TenantWriteError::IdConflict));
    assert_eq!(err.wire_code(), "23505");

    let stored = storage
        .get_row_from_table(TABLE, TENANT_A, 7)
        .expect("row must still exist");
    assert_eq!(
        stored.metadata,
        b"first".to_vec(),
        "a rejected duplicate must not overwrite the existing row"
    );
}

// (c) 対象ビヘイビア: RLS-9。他テナントに同一 id の行が「ある場合」と「ない場合」とで、
// 自テナント名義 INSERT の応答（成否・`wire_code`・エラー文言）が完全に一致すること。
// レイテンシ差は本テストでは判定しない（閾値・試行回数は spec 側の宿題。実装上は
// 物理キーの名前空間化により他テナント行を参照する分岐が存在しないため、経路自体が同一）。
#[test]
fn rls9_insert_response_is_identical_whether_or_not_another_tenant_holds_the_id() {
    // ケース 1: 他テナントが id=42 を保持している状態で tenant-a が id=42 を INSERT。
    let (storage_with, _cleanup_with) = open_seeded("resp-with-foreign");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    tenant::insert_row(
        &storage_with,
        TABLE,
        &b,
        42,
        &row(TENANT_B, &[0.0, 1.0], b"foreign"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed tenant-b row id=42");
    let with_foreign = tenant::insert_row(
        &storage_with,
        TABLE,
        &a,
        42,
        &row(TENANT_A, &[1.0, 0.0], b"mine"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    );

    // ケース 2: どのテナントも id=42 を保持していない状態で同じ INSERT。
    let (storage_without, _cleanup_without) = open_seeded("resp-without-foreign");
    let without_foreign = tenant::insert_row(
        &storage_without,
        TABLE,
        &a,
        42,
        &row(TENANT_A, &[1.0, 0.0], b"mine"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    );

    assert!(
        with_foreign.is_ok() && without_foreign.is_ok(),
        "both cases must take the same successful path (RLS-9)"
    );
    assert_eq!(
        format!("{with_foreign:?}"),
        format!("{without_foreign:?}"),
        "responses must be indistinguishable between the two cases"
    );

    // 逆方向（同一テナント内重複）でも、他テナント行の有無で応答が変わらないこと。
    // TASK-94・RECOVER-3: `with_foreign`/`without_foreign` が既に tenant-a/TABLE で
    // `"test-op"` を使っているため、ここで同じ値を再利用すると台帳の重複拒否
    // （`DuplicateOperationId`）に化けてしまい、本テストが検証したい行キー衝突
    // （`IdConflict`）を通らなくなる。未使用の別 `operation_id` を使う。
    let dup_with_foreign = tenant::insert_row(
        &storage_with,
        TABLE,
        &a,
        42,
        &row(TENANT_A, &[1.0, 0.0], b"dup"),
        &engine::recovery::required_op_id::OperationId::parse("test-op-dup")
            .expect("valid operation_id"),
    )
    .expect_err("duplicate within tenant-a must be rejected");
    let dup_without_foreign = tenant::insert_row(
        &storage_without,
        TABLE,
        &a,
        42,
        &row(TENANT_A, &[1.0, 0.0], b"dup"),
        &engine::recovery::required_op_id::OperationId::parse("test-op-dup")
            .expect("valid operation_id"),
    )
    .expect_err("duplicate within tenant-a must be rejected");
    assert!(matches!(dup_with_foreign, TenantWriteError::IdConflict));
    assert!(matches!(dup_without_foreign, TenantWriteError::IdConflict));
    assert_eq!(
        dup_with_foreign.wire_code(),
        dup_without_foreign.wire_code()
    );
    assert_eq!(
        format!("{dup_with_foreign}"),
        format!("{dup_without_foreign}")
    );
    assert_eq!(
        format!("{dup_with_foreign:?}"),
        format!("{dup_without_foreign:?}")
    );
    // 応答文言に他テナント名・id が現れないこと（存在情報秘匿の回帰検証）。
    let text = format!("{dup_with_foreign} {dup_with_foreign:?}");
    assert!(!text.contains(TENANT_B), "response leaked a tenant: {text}");
    assert!(!text.contains("42"), "response leaked the row id: {text}");
}

// (d) 対象ビヘイビア: RECOVER-4。越境 UPDATE/DELETE は、対象 id が他テナントに
// 存在する場合も存在しない場合も一律 `NotFound` に統一される。
#[test]
fn recover4_cross_tenant_update_and_delete_are_uniformly_not_found() {
    let (storage, _cleanup) = open_seeded("cross-update-delete");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    tenant::insert_row(
        &storage,
        TABLE,
        &b,
        5,
        &row(TENANT_B, &[0.0, 1.0], b"b5"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("seed tenant-b row id=5");

    let update_existing = tenant::update_row(
        &storage,
        TABLE,
        &a,
        5,
        &row(TENANT_A, &[1.0, 0.0], b"x"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("cross-tenant update must fail");
    let update_missing = tenant::update_row(
        &storage,
        TABLE,
        &a,
        6,
        &row(TENANT_A, &[1.0, 0.0], b"x"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("update of a nonexistent id must fail");
    let delete_existing = tenant::delete_row(
        &storage,
        TABLE,
        &a,
        5,
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("cross-tenant delete must fail");
    let delete_missing = tenant::delete_row(
        &storage,
        TABLE,
        &a,
        6,
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect_err("delete of a nonexistent id must fail");

    for e in [
        &update_existing,
        &update_missing,
        &delete_existing,
        &delete_missing,
    ] {
        assert!(matches!(e, TenantWriteError::NotFound));
        assert_eq!(e.wire_code(), "P0002");
    }
    assert_eq!(
        format!("{update_existing} {update_existing:?}"),
        format!("{update_missing} {update_missing:?}"),
        "the presence of another tenant's row must not change the response"
    );

    // 他テナントの行が試行後も不変であること。
    let victim = storage
        .get_row_from_table(TABLE, TENANT_B, 5)
        .expect("tenant-b row must still exist");
    assert_eq!(victim.metadata, b"b5".to_vec());
}

// (e) 対象ビヘイビア: TABLE-12 の読み取り経路への波及。異なるテナントの `Public` 行が
// 同一 id を持つ構成でも検索が fail-closed に落ちず（`core::provider_result_is_valid` の
// 多重集合判定）、両方の可視行が結果に現れること。集合ベースの重複拒否へ戻すと、
// 他テナントが同じ id の `Public` 行を作るだけで検索が失敗する（テナント間の可用性干渉に
// なる）ため、その退行を防ぐ回帰テストでもある。
#[test]
fn table12_search_tolerates_the_same_id_held_by_two_tenants_as_public_rows() {
    let (storage, _cleanup) = open_seeded("search-dup-id");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    tenant::insert_row(
        &storage,
        TABLE,
        &a,
        1,
        &row(TENANT_A, &[1.0, 0.0], b"a1"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-a public row id=1");
    tenant::insert_row(
        &storage,
        TABLE,
        &b,
        1,
        &row(TENANT_B, &[1.0, 0.0], b"b1"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-b public row id=1");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let hits = core
        .search(&a, TABLE, &[1.0, 0.0], 10)
        .expect("search must not fail merely because two tenants share a row id");
    assert_eq!(
        hits.len(),
        2,
        "both visible rows (own row and the other tenant's Public row) must be returned"
    );
    assert!(hits.iter().all(|h| h.id == 1));
}

// (f) 対象ビヘイビア: TABLE-12 の読み取り経路への波及（Bugbot High 指摘・PR #194）。
// 同一 `id` の可視行が 2 テナント分あるとき、SQL 投影が「あるテナントの embedding と
// 別テナントのスカラー列」を混ぜて返さないこと。`sql/exec.rs` が行の同定を行 `id` から
// アリーナのスロット番号へ切り替えたことの回帰テスト（id ベースのままだと
// embedding は先勝ち・スカラー列は後勝ちで解決され、両者が食い違う）。
#[test]
fn table12_sql_projection_never_mixes_embedding_and_scalars_across_tenants() {
    let path = unique_db_path("sql-mix");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    // 双方 id=100（スロット番号 0/1 とずれる値を選ぶ。投影が本来の行 id を返すことも
    // ここで固定する）。embedding と body はテナントごとに別の値にする。
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &a,
        100,
        Visibility::Public,
        &[
            RowCodecValue::Vector(vec![1.0, 0.0]),
            RowCodecValue::Text("body-a".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-a row id=100");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &b,
        100,
        Visibility::Public,
        &[
            RowCodecValue::Vector(vec![0.0, 1.0]),
            RowCodecValue::Text("body-b".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-b row id=100");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let result = core
        .execute_sql(
            &a,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect("select must succeed with a duplicated row id across tenants");
    assert_eq!(result.rows.len(), 2, "both visible rows must be returned");
    for row in &result.rows {
        assert_eq!(row.id, 100, "projection must return the real row id");
        // embedding と body は必ず同じ行（同じテナント）由来であること。
        let embedding = row.cells.get(1).cloned();
        let body = row.cells.get(2).cloned();
        match (embedding, body) {
            (Some(Cell::Vector(v)), Some(Cell::Text(t))) => {
                let expected_body = if v == vec![1.0, 0.0] {
                    "body-a"
                } else if v == vec![0.0, 1.0] {
                    "body-b"
                } else {
                    panic!("unexpected embedding: {v:?}")
                };
                assert_eq!(
                    t, expected_body,
                    "embedding and scalar columns must come from the same row"
                );
            }
            other => panic!("unexpected projection cells: {other:?}"),
        }
    }
}

// (g) 対象ビヘイビア: TABLE-12（Bugbot Medium 指摘・PR #194）。ハイブリッド検索
// （疎コーパスを構築する経路）でも、同一 `id` の可視行が 2 テナント分あるときに
// `SparseIndex::build` の `DuplicateDocId` で失敗せず、両行が独立に扱われること。
#[test]
fn table12_hybrid_sql_tolerates_the_same_id_held_by_two_tenants() {
    let path = unique_db_path("sql-hybrid-dup");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &a,
        100,
        Visibility::Public,
        &[
            RowCodecValue::Vector(vec![1.0, 0.0]),
            RowCodecValue::Text("vector database tenant a".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-a row id=100");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &b,
        100,
        Visibility::Public,
        &[
            RowCodecValue::Vector(vec![0.0, 1.0]),
            RowCodecValue::Text("vector database tenant b".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-b row id=100");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let result = core
        .execute_sql(
            &a,
            "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 10",
        )
        .expect("hybrid select must succeed with a duplicated row id across tenants");
    assert_eq!(
        result.rows.len(),
        2,
        "both visible rows must be fused independently"
    );
    let bodies: Vec<String> = result
        .rows
        .iter()
        .map(|r| match r.cells.get(2) {
            Some(Cell::Text(t)) => t.clone(),
            other => panic!("unexpected body cell: {other:?}"),
        })
        .collect();
    assert!(bodies.contains(&"vector database tenant a".to_string()));
    assert!(bodies.contains(&"vector database tenant b".to_string()));
}

// (h) 対象ビヘイビア: RECOVER-4・TABLE-12（codex-review P0 指摘・PR #194）。
// テナント境界付きバッチ API（`tenant::insert_rows`）の認可・重複契約。
#[test]
fn recover4_guarded_batch_insert_enforces_tenant_and_id_contracts() {
    let (storage, _cleanup) = open_seeded("guarded-batch");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);

    // 他テナント名義の行が 1 件でも混ざるバッチは Forbidden（1 件も書かれない）。
    let mixed = vec![
        (1u64, row(TENANT_A, &[1.0, 0.0], b"a1")),
        (2u64, row(TENANT_B, &[0.0, 1.0], b"b2")),
    ];
    let err = engine::tenant::insert_rows(
        &storage,
        TABLE,
        &a,
        &mixed,
        &engine::recovery::required_op_id::OperationId::parse("test-op-mixed")
            .expect("valid operation_id"),
    )
    .expect_err("a batch containing another tenant's row must be rejected");
    assert!(matches!(err, TenantWriteError::Forbidden));
    assert_eq!(err.wire_code(), "42501");
    assert!(storage.get_row_from_table(TABLE, TENANT_A, 1).is_err());

    // バッチ内の id 重複は IdConflict（後勝ちで先行行を黙って上書きしない）。
    let dup = vec![
        (5u64, row(TENANT_A, &[1.0, 0.0], b"first")),
        (5u64, row(TENANT_A, &[0.0, 1.0], b"second")),
    ];
    let err = engine::tenant::insert_rows(
        &storage,
        TABLE,
        &a,
        &dup,
        &engine::recovery::required_op_id::OperationId::parse("test-op-dup")
            .expect("valid operation_id"),
    )
    .expect_err("duplicate ids within one batch must be rejected");
    assert!(matches!(err, TenantWriteError::IdConflict));
    assert!(storage.get_row_from_table(TABLE, TENANT_A, 5).is_err());

    // 正常系: 自テナント名義のバッチは成功し、他テナントは同じ id を独立に使える。
    // TASK-94・RECOVER-3: 台帳の重複拒否が入ったため、各バッチ呼び出しに別の
    // `operation_id` を使う（固定文言の使い回しは同一テナント内の 2 回目以降が
    // `23505` になる）。
    let batch_a = vec![
        (1u64, row(TENANT_A, &[1.0, 0.0], b"a1")),
        (2u64, row(TENANT_A, &[0.5, 0.5], b"a2")),
    ];
    engine::tenant::insert_rows(
        &storage,
        TABLE,
        &a,
        &batch_a,
        &engine::recovery::required_op_id::OperationId::parse("test-op-batch-a")
            .expect("valid operation_id"),
    )
    .expect("own-tenant batch ok");
    let batch_b = vec![(1u64, row(TENANT_B, &[0.0, 1.0], b"b1"))];
    engine::tenant::insert_rows(
        &storage,
        TABLE,
        &b,
        &batch_b,
        &engine::recovery::required_op_id::OperationId::parse("test-op-batch-b")
            .expect("valid operation_id"),
    )
    .expect("another tenant may reuse the same id");
    assert_eq!(
        storage
            .get_row_from_table(TABLE, TENANT_A, 1)
            .expect("tenant-a row")
            .metadata,
        b"a1".to_vec()
    );
    assert_eq!(
        storage
            .get_row_from_table(TABLE, TENANT_B, 1)
            .expect("tenant-b row")
            .metadata,
        b"b1".to_vec()
    );

    // 既存行と衝突するバッチは IdConflict（既存行は不変）。TASK-101（RECOVER-10）:
    // 台帳照合が行書き込みより前に行われるため、`batch_a` と同じ operation_id を
    // 別内容のバッチへ再利用すると OperationIdContentMismatch が先に検出されて
    // しまう。行 id レベルの衝突検出を独立に確認するため別の operation_id を使う。
    let conflict = vec![(2u64, row(TENANT_A, &[0.0, 0.0], b"overwrite"))];
    let err = engine::tenant::insert_rows(
        &storage,
        TABLE,
        &a,
        &conflict,
        &engine::recovery::required_op_id::OperationId::parse("test-op-conflict")
            .expect("valid operation_id"),
    )
    .expect_err("existing id within the same tenant must conflict");
    assert!(matches!(err, TenantWriteError::IdConflict));
    assert_eq!(
        storage
            .get_row_from_table(TABLE, TENANT_A, 2)
            .expect("tenant-a row 2")
            .metadata,
        b"a2".to_vec()
    );
}

// (i) 対象ビヘイビア: TABLE-12・RLS-9（codex-review P1 指摘・PR #194）。
// `tenant::verify_hits` は `(tenant_id, id)` の完全な行キーで照合する。
// 3 テナント構成にするのが要点で、「可視な行（tenant-b の `Public` 行 id=1）と
// 同じ `id` を持つ不可視行（tenant-c の `Private` 行 id=1）」由来のヒットは、
// id だけの集合照合では可視集合に含まれてしまい見逃される。
#[test]
fn rls9_verify_hits_detects_an_invisible_row_sharing_an_id_with_a_visible_row() {
    let (storage, _cleanup) = open_seeded("verify-hits-row-key");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    let c = ctx(TENANT_C);
    // tenant-b の Public 行（tenant-a から可視）と、同じ id を持つ tenant-c の
    // Private 行（tenant-a から不可視）。
    engine::tenant::insert_row(
        &storage,
        TABLE,
        &b,
        1,
        &row(TENANT_B, &[1.0, 0.0], b"b1"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-b public row id=1");
    engine::tenant::insert_row(
        &storage,
        TABLE,
        &c,
        1,
        &RowInput {
            tenant_id: TENANT_C,
            visibility: Visibility::Private,
            embedding: &[0.0, 1.0],
            metadata: b"c1",
        },
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-c private row id=1");

    // 検索用 ctx は tenant-a（Public のみ許可）。
    let viewer = PolicyContext::new(TENANT_A).expect("valid tenant");
    let _ = &a;

    // 可視行由来のヒットは受理される。
    let visible_hit = SearchHit::new(TENANT_B, 1, 1.0);
    engine::tenant::verify_hits(&storage, TABLE, &viewer, std::slice::from_ref(&visible_hit))
        .expect("a hit on a visible row must be accepted");

    // 不可視行（tenant-c の Private 行）由来のヒットは、`id` が可視集合に存在していても
    // 拒否されなければならない（id だけの照合では見逃される退行の防止）。
    let invisible_hit = SearchHit::new(TENANT_C, 1, 1.0);
    let err = engine::tenant::verify_hits(
        &storage,
        TABLE,
        &viewer,
        std::slice::from_ref(&invisible_hit),
    )
    .expect_err("a hit on an invisible row that shares an id with a visible row must be rejected");
    assert!(matches!(err, TenantError::HitOutsideVisibleSet));
}

// (j) 対象ビヘイビア: TABLE-12・RLS-9（codex-review P1 指摘・PR #194）。
// 検索結果 `SearchHit` は `(tenant_id, id)` で行を一意に解決でき、他テナントの
// `Public` 行のヒットが同 id の自テナント行へ解決されない。
#[test]
fn table12_search_hits_resolve_to_the_exact_row_via_tenant_and_id() {
    let (storage, _cleanup) = open_seeded("hit-resolution");
    let a = ctx(TENANT_A);
    let b = ctx(TENANT_B);
    engine::tenant::insert_row(
        &storage,
        TABLE,
        &a,
        1,
        &row(TENANT_A, &[1.0, 0.0], b"a1"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-a row id=1");
    engine::tenant::insert_row(
        &storage,
        TABLE,
        &b,
        1,
        &row(TENANT_B, &[0.9, 0.1], b"b1"),
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("tenant-b public row id=1");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let viewer = PolicyContext::new(TENANT_A).expect("valid tenant");
    let hits = core
        .search(&viewer, TABLE, &[1.0, 0.0], 10)
        .expect("search ok");
    assert_eq!(hits.len(), 2);
    // 各ヒットは自分自身の行へ解決される（テナントを取り違えない）。
    for hit in &hits {
        let row = core
            .get_row(&viewer, TABLE, &hit.tenant_id, hit.id)
            .expect("hit must resolve to an existing, visible row");
        assert_eq!(row.tenant_id, hit.tenant_id);
        assert_eq!(row.id, hit.id);
        let expected_metadata: &[u8] = if hit.tenant_id == TENANT_A {
            b"a1"
        } else {
            b"b1"
        };
        assert_eq!(row.metadata, expected_metadata.to_vec());
    }
    // 両テナントのヒットが 1 件ずつ含まれること（同一 id が畳み込まれていない）。
    let mut tenants: Vec<&str> = hits.iter().map(|h| h.tenant_id.as_str()).collect();
    tenants.sort_unstable();
    assert_eq!(tenants, vec![TENANT_A, TENANT_B]);
}
