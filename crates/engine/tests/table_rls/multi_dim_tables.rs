//! 複数次元テーブル共存の回帰テスト（TASK-91、対象ビヘイビア: TABLE-2。
//! ポインタ: `docs/spec/05-tasks.md` TASK-91・`docs/spec/04-behavior/data-model.md`。
//! 関連: EXT-2（`docs/spec/04-behavior/extensions.md`））。
//!
//! `crates/engine/tests/catalog.rs`（TASK-85）は単一次元のみを前提にカタログ DDL
//! の正しさを検証する。本ファイルは、異なる次元を宣言した複数テーブルを単一
//! `Storage`（同一 redb ファイル）へ `create_table` した上で、カタログ層
//! （`catalog.rs`）・行ストア層（`storage.rs`）の双方にわたって既存 API の
//! 挙動を検証する TASK-91 固有の回帰テストである。
//!
//! スコープ境界（重要）: 行とユーザーテーブル（カタログ上のテーブル名）を関連付ける
//! 機構は本リポジトリにまだ実装されていない（後続タスクの管轄）。そのため本ファイルの
//! 行ストア系テストでは、テーブルごとに素の id レンジを分け、`RowInput::metadata` に
//! テーブル名を記録することで、テスト側の判断としてテーブル帰属を模擬する
//! （`docs/design/multi-dim-table-coexistence.md` の「制約」節も参照）。
//! production コード（`src/`）の変更はスコープ外。
//!
//! ヘルパ（`unique_db_path` / `CleanupGuard`）は Issue #173 で
//! `crates/engine/src/test_util/temp_db.rs` へ一本化した。決定論的な埋め込み生成は
//! このファイル固有のまま複製する（`tests/incremental_write_perf.rs` の既存方針を踏襲）。

use engine::catalog::{CatalogError, ColumnDef, ColumnType, TableSchema};
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 全行に付与するダミーのテナント識別子（本テストはテナント境界の判定経路に
/// 踏み込まない。空文字列は `RowInput` 側で拒否されるため非空の固定値を使う）。
const TENANT_ID: &str = "tenant-a";

/// 検証対象の 3 次元（一般的な埋め込みモデルで使われる代表値。TASK-91 上、
/// データセット選定は人間の判断事項のため、決定論的な合成ベクトルを既定値とする）。
const DIMS: [u32; 3] = [384, 768, 1536];

/// 外部クレート非依存の決定的擬似乱数生成器（xorshift32）。テストデータ生成にのみ使う。
struct Xorshift32(u32);

impl Xorshift32 {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    fn next_f32(&mut self) -> f32 {
        (self.next_u32() as f64 / u32::MAX as f64) as f32
    }
}

/// `dim` 次元・`seed` に基づく決定論的な埋め込みを生成する。
fn make_embedding(dim: u32, seed: u32) -> Vec<f32> {
    let mut rng = Xorshift32(seed | 1); // seed 0 は xorshift の不動点になるため奇数化する。
    (0..dim).map(|_| rng.next_f32()).collect()
}

/// テーブル名と行 ID をエンコードしたメタデータ（本ファイル固有のテスト判断。
/// production の行ストアはテーブル関連付けを持たないため、テストが独自に付与する）。
fn make_metadata(table_name: &str, id: u64) -> Vec<u8> {
    format!("table={table_name};id={id}").into_bytes()
}

/// `table_name` の schema を組み立てる（`embedding` 列 1 本のみを持つ最小構成）。
fn schema_for(table_name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        table_name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
    )
}

/// 各テーブルの id レンジ（[base, base+PER_TABLE_ROWS) ）が重ならないよう、
/// テーブルインデックスごとに大きく間隔を空ける。
const PER_TABLE_ROWS: u64 = 20;
const ID_RANGE_STRIDE: u64 = 1_000_000;

fn id_base_for(table_idx: usize) -> u64 {
    table_idx as u64 * ID_RANGE_STRIDE
}

/// 対象ビヘイビア: TABLE-2。異なる次元を宣言した 3 テーブルを単一 `Storage` へ
/// `create_table` し、`list_tables` / `get_table_schema` それぞれから次元宣言を
/// 正しく読み戻せることを検証する。
#[test]
fn create_tables_with_distinct_dims_coexist() {
    let path = unique_db_path("create-coexist");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let table_names = ["docs_384", "docs_768", "docs_1536"];
    for (name, dim) in table_names.iter().zip(DIMS.iter()) {
        storage
            .create_table(&schema_for(name, *dim))
            .unwrap_or_else(|e| panic!("create_table({name}) should succeed: {e}"));
    }

    let mut listed = storage.list_tables().expect("list_tables");
    listed.sort();
    let mut expected: Vec<String> = table_names.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(listed, expected);

    for (name, dim) in table_names.iter().zip(DIMS.iter()) {
        let schema = storage
            .get_table_schema(name)
            .unwrap_or_else(|e| panic!("get_table_schema({name}) should succeed: {e}"));
        assert_eq!(schema.vector_dim(), Some(*dim));
    }
}

/// 対象ビヘイビア: TABLE-2。3 テーブルを作成した DB を close → 再 `open`
/// しても、各テーブルの次元宣言が不変であることを検証する。
#[test]
fn schemas_survive_reopen() {
    let path = unique_db_path("reopen");
    let _guard = CleanupGuard(path.clone());

    let table_names = ["docs_384", "docs_768", "docs_1536"];
    {
        let storage = Storage::open(&path).expect("open storage (first)");
        for (name, dim) in table_names.iter().zip(DIMS.iter()) {
            storage
                .create_table(&schema_for(name, *dim))
                .unwrap_or_else(|e| panic!("create_table({name}) should succeed: {e}"));
        }
        // 明示的に drop してファイルハンドルを閉じ、再オープンが独立した読み出しに
        // なるようにする。
        drop(storage);
    }

    let storage = Storage::open(&path).expect("open storage (reopen)");
    for (name, dim) in table_names.iter().zip(DIMS.iter()) {
        let schema = storage.get_table_schema(name).unwrap_or_else(|e| {
            panic!("get_table_schema({name}) should succeed after reopen: {e}")
        });
        assert_eq!(schema.vector_dim(), Some(*dim));
    }
}

/// 対象ビヘイビア: TABLE-2。各テーブルの `validate_embedding_dim` が自テーブルの
/// 次元のみを受理し、他テーブルの次元・0・上限超過を拒否することを検証する
/// （テーブル間の次元混同を防ぐ境界）。
#[test]
fn dim_validation_is_fail_closed() {
    let path = unique_db_path("dim-validation");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let table_names = ["docs_384", "docs_768", "docs_1536"];
    for (name, dim) in table_names.iter().zip(DIMS.iter()) {
        storage
            .create_table(&schema_for(name, *dim))
            .unwrap_or_else(|e| panic!("create_table({name}) should succeed: {e}"));
    }

    for (name, own_dim) in table_names.iter().zip(DIMS.iter()) {
        let schema = storage.get_table_schema(name).expect("get_table_schema");

        // 自テーブルの次元は受理する。
        assert!(schema.validate_embedding_dim(*own_dim as usize).is_ok());

        // 他テーブルの次元はすべて拒否する。
        for other_dim in DIMS.iter().filter(|d| *d != own_dim) {
            assert!(
                schema.validate_embedding_dim(*other_dim as usize).is_err(),
                "table {name} (dim={own_dim}) must reject foreign dim {other_dim}"
            );
        }

        // 0・上限超過（u32::MAX）も拒否する。
        assert!(schema.validate_embedding_dim(0).is_err());
        assert!(schema.validate_embedding_dim(u32::MAX as usize).is_err());
    }
}

/// 対象ビヘイビア: TABLE-2。3 テーブル分の行を id レンジで分離して同一 redb
/// ファイルへ混在格納し、`scan_page` で全行を読み切って件数・embedding 長・値・
/// metadata が完全一致すること（混線 0 件）を検証する。再オープン後にも同一検証
/// を行い、永続化後も破損・混線がないことを確認する。
#[test]
fn mixed_dim_rows_roundtrip_intact() {
    let path = unique_db_path("mixed-rows");
    let _guard = CleanupGuard(path.clone());

    let table_names = ["docs_384", "docs_768", "docs_1536"];

    // テーブルごとの行データを事前生成する（`RowInput` の借用元として保持する）。
    struct OwnedRow {
        id: u64,
        table_name: &'static str,
        embedding: Vec<f32>,
        metadata: Vec<u8>,
    }
    let mut owned_rows: Vec<OwnedRow> = Vec::new();
    for (table_idx, (name, dim)) in table_names.iter().zip(DIMS.iter()).enumerate() {
        let base = id_base_for(table_idx);
        for offset in 0..PER_TABLE_ROWS {
            let id = base + offset;
            let seed = (id as u32).wrapping_mul(0x9e37_79b9).wrapping_add(1);
            owned_rows.push(OwnedRow {
                id,
                table_name: name,
                embedding: make_embedding(*dim, seed),
                metadata: make_metadata(name, id),
            });
        }
    }

    {
        let storage = Storage::open(&path).expect("open storage (write)");
        for (name, dim) in table_names.iter().zip(DIMS.iter()) {
            storage
                .create_table(&schema_for(name, *dim))
                .unwrap_or_else(|e| panic!("create_table({name}) should succeed: {e}"));
        }

        let batch: Vec<(u64, RowInput<'_>)> = owned_rows
            .iter()
            .map(|r| {
                (
                    r.id,
                    RowInput {
                        tenant_id: TENANT_ID,
                        visibility: Visibility::Public,
                        embedding: &r.embedding,
                        metadata: &r.metadata,
                    },
                )
            })
            .collect();
        storage
            .put_batch(&batch)
            .expect("put_batch across mixed-dim tables should succeed");

        assert_mixed_rows_intact(&storage, &owned_rows);
    }

    // 再オープン後も同一検証を行う（永続化を跨いだ混線がないことを確認する）。
    let storage = Storage::open(&path).expect("open storage (reopen)");
    assert_mixed_rows_intact(&storage, &owned_rows);

    struct RowRef<'a> {
        id: u64,
        table_name: &'a str,
        embedding: &'a [f32],
        metadata: &'a [u8],
    }

    fn assert_mixed_rows_intact(storage: &Storage, owned_rows: &[OwnedRow]) {
        let refs: Vec<RowRef<'_>> = owned_rows
            .iter()
            .map(|r| RowRef {
                id: r.id,
                table_name: r.table_name,
                embedding: &r.embedding,
                metadata: &r.metadata,
            })
            .collect();
        assert_rows_match(storage, &refs);
    }

    fn assert_rows_match(storage: &Storage, expected: &[RowRef<'_>]) {
        // 上限付きページングで全行を読み切る（`scan()` ではなく `scan_page` を使う方針。
        // `docs/design/concurrent-write-verification.md` の既存知見に従う）。
        // ページサイズは総行数（PER_TABLE_ROWS * テーブル数）未満に固定し、cursor が
        // 複数ページへ跨って進行する経路を実際に通す。
        let mut all_rows = Vec::new();
        let mut cursor: Option<engine::storage::RowCursor> = None;
        loop {
            let cursor_ref = cursor.as_ref().map(|(t, id)| (t.as_str(), *id));
            let (page, next_cursor) = storage
                .scan_page(cursor_ref, 16)
                .expect("scan_page should succeed");
            all_rows.extend(page);
            match next_cursor {
                None => break,
                // 安全弁: cursor が前進しない（実装上は発生しないはずだが、将来の実装変更で
                // 壊れた場合に無限ループへ陥らないための防御）場合は打ち切る。
                Some(ref next) if cursor.as_ref().is_some_and(|prev| next <= prev) => break,
                Some(next) => cursor = Some(next),
            }
        }

        assert_eq!(
            all_rows.len(),
            expected.len(),
            "row count must match exactly (no cross-table contamination or loss)"
        );

        let mut by_id: std::collections::HashMap<u64, engine::storage::Row> =
            all_rows.into_iter().map(|r| (r.id, r)).collect();

        for exp in expected {
            let row = by_id
                .remove(&exp.id)
                .unwrap_or_else(|| panic!("row id={} must be present", exp.id));
            assert_eq!(row.embedding.len(), exp.embedding.len(), "id={}", exp.id);
            assert_eq!(
                row.embedding, exp.embedding,
                "embedding must round-trip exactly for id={} (table={})",
                exp.id, exp.table_name
            );
            assert_eq!(
                row.metadata, exp.metadata,
                "metadata must round-trip exactly for id={} (table={})",
                exp.id, exp.table_name
            );
        }
        assert!(
            by_id.is_empty(),
            "no unexpected extra rows should be present, found: {:?}",
            by_id.keys().collect::<Vec<_>>()
        );
    }
}

/// 対象ビヘイビア: TABLE-2/TABLE-5。1 テーブルへ `alter_table_add_column` した後も
/// 他テーブルの次元宣言・既存行が不変であることを検証する（カタログ変更が
/// テーブル境界を越えて他テーブルへ波及しないことの確認）。
#[test]
fn alter_table_does_not_disturb_other_dims() {
    let path = unique_db_path("alter-isolation");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let table_names = ["docs_384", "docs_768", "docs_1536"];
    for (name, dim) in table_names.iter().zip(DIMS.iter()) {
        storage
            .create_table(&schema_for(name, *dim))
            .unwrap_or_else(|e| panic!("create_table({name}) should succeed: {e}"));
    }

    // 1 行だけ各テーブルに書き込み、alter 後も既存行が不変であることの対象とする。
    let mut existing_rows = Vec::new();
    for (table_idx, (name, dim)) in table_names.iter().zip(DIMS.iter()).enumerate() {
        let id = id_base_for(table_idx);
        let embedding = make_embedding(*dim, (id as u32).wrapping_add(7));
        let metadata = make_metadata(name, id);
        storage
            .put(
                id,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &embedding,
                    metadata: &metadata,
                },
            )
            .expect("put should succeed");
        existing_rows.push((id, embedding, metadata));
    }

    // docs_768 のみへ列を追加する。
    storage
        .alter_table_add_column("docs_768", ColumnDef::new("tag", ColumnType::Text, true))
        .expect("alter_table_add_column should succeed");

    // 他テーブル（docs_384, docs_1536）の次元宣言は不変。
    let schema_384 = storage.get_table_schema("docs_384").expect("get schema");
    assert_eq!(schema_384.vector_dim(), Some(DIMS[0]));
    assert_eq!(
        schema_384.columns.len(),
        1,
        "docs_384 must not gain columns"
    );

    let schema_1536 = storage.get_table_schema("docs_1536").expect("get schema");
    assert_eq!(schema_1536.vector_dim(), Some(DIMS[2]));
    assert_eq!(
        schema_1536.columns.len(),
        1,
        "docs_1536 must not gain columns"
    );

    // docs_768 自体は列が増え、次元宣言は不変。
    let schema_768 = storage.get_table_schema("docs_768").expect("get schema");
    assert_eq!(schema_768.vector_dim(), Some(DIMS[1]));
    assert_eq!(schema_768.columns.len(), 2, "docs_768 must gain one column");

    // 既存行はすべて不変であることを確認する。
    for (id, embedding, metadata) in &existing_rows {
        let row = storage
            .get(TENANT_ID, *id)
            .expect("get existing row after alter");
        assert_eq!(&row.embedding, embedding, "id={id}");
        assert_eq!(&row.metadata, metadata, "id={id}");
    }

    // 重複 alter（同名列）は fail-closed に拒否される（TABLE-5 契約の確認）。
    let dup_result =
        storage.alter_table_add_column("docs_768", ColumnDef::new("tag", ColumnType::Text, true));
    assert!(matches!(
        dup_result,
        Err(CatalogError::ColumnAlreadyExists(_))
    ));
}
