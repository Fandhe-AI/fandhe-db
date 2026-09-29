//! `engine::catalog` の統合テスト（TASK-85、対象ビヘイビア: TABLE-1, TABLE-4,
//! TABLE-5, TABLE-6。ポインタ: `docs/spec/04-behavior/data-model.md`）。
//!
//! ヘルパ（`unique_db_path` / `CleanupGuard`）は Issue #173 で
//! `crates/engine/src/test_util/temp_db.rs` へ一本化した（旧: 結合テストごとの複製）。

use std::time::Instant;

use redb::{ReadableDatabase, ReadableTable};

use engine::catalog::{CatalogError, ColumnDef, ColumnType, TableSchema};
use engine::storage::{RowInput, Storage, Visibility};

/// `storage.rs::ROWS_TABLE` と同一のテーブル定義（`pub(crate)` のため本クレート外の
/// ここでは参照できず、`tests/persistence.rs` と同じ流儀でローカルに再宣言する）。
/// 物理キーは `(tenant_id, id)` の複合キー（対象ビヘイビア: TABLE-12）。
/// 行データの生バイト列を検証するテスト（TABLE-4/TABLE-5）でのみ使う。
const ROWS_TABLE: redb::TableDefinition<(&str, u64), &[u8]> = redb::TableDefinition::new("rows");

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn embedding_schema(name: &str, dim: u32) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn row<'a>(embedding: &'a [f32], metadata: &'a [u8]) -> RowInput<'a> {
    RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding,
        metadata,
    }
}

// --- TABLE-1 -----------------------------------------------------------

#[test]
fn table1_schema_roundtrip_preserves_declared_dimension() {
    let path = unique_db_path("table1-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let schema = embedding_schema("docs", 384);
    storage.create_table(&schema).expect("create_table");

    let loaded = storage.get_table_schema("docs").expect("get_table_schema");
    assert_eq!(loaded.vector_dim(), Some(384));
    assert_eq!(loaded, schema);
}

#[test]
fn table1_validate_embedding_dim_accepts_match_and_rejects_mismatch() {
    let path = unique_db_path("table1-validate-dim");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let schema = embedding_schema("docs", 128);
    storage.create_table(&schema).expect("create_table");
    let loaded = storage.get_table_schema("docs").expect("get_table_schema");

    assert!(loaded.validate_embedding_dim(128).is_ok());
    assert!(loaded.validate_embedding_dim(64).is_err());
    assert!(loaded.validate_embedding_dim(256).is_err());
}

#[test]
fn table1_rejects_second_vector_column_with_different_dimension() {
    // 複数の VECTOR 列を許すと TableSchema::vector_dim() が先頭列のみを見て
    // 後続列を黙殺してしまう（fail-open）ため、CREATE TABLE 時点で拒否する。
    let path = unique_db_path("table1-reject-multi-vector");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(384), false),
            ColumnDef::new("other_embedding", ColumnType::Vector(8), false),
        ],
    );
    assert!(matches!(
        storage.create_table(&schema),
        Err(CatalogError::Invalid(_))
    ));

    // ALTER TABLE ADD COLUMN 経由で 2 本目の VECTOR 列を追加しようとしても同様に拒否される。
    storage
        .create_table(&embedding_schema("docs2", 384))
        .expect("create_table single vector column");
    let result = storage.alter_table_add_column(
        "docs2",
        ColumnDef::new("other_embedding", ColumnType::Vector(8), true),
    );
    assert!(matches!(result, Err(CatalogError::Invalid(_))));
}

#[test]
fn table1_multiple_tables_with_different_dimensions_coexist() {
    let path = unique_db_path("table1-coexist");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&embedding_schema("docs_small", 64))
        .expect("create_table small");
    storage
        .create_table(&embedding_schema("docs_large", 1536))
        .expect("create_table large");

    let small = storage.get_table_schema("docs_small").expect("get small");
    let large = storage.get_table_schema("docs_large").expect("get large");
    assert_eq!(small.vector_dim(), Some(64));
    assert_eq!(large.vector_dim(), Some(1536));
}

// --- TABLE-4 -------------------------------------------------------------

/// 指定行数のダミー行を書き込んだ DB を用意する（性能検証の前提データ）。
fn seed_rows(storage: &Storage, count: u64) {
    let embedding = vec![0.5_f32; 8];
    let metadata = b"row".to_vec();
    let rows: Vec<(u64, RowInput<'_>)> = (0..count)
        .map(|id| (id, row(&embedding, &metadata)))
        .collect();
    storage.put_batch(&rows).expect("seed put_batch");
}

/// `ROWS_TABLE` の全エントリを生バイト列のまま読み出す。呼び出し前提として、
/// この時点で `Storage`（＝同一ファイルの `redb::Database` ハンドル）が
/// drop 済みであること（redb はファイルロックの都合上、同一ファイルへの
/// 複数ハンドル同時オープンを許さない。`tests/persistence.rs` と同じ制約）。
/// `Storage::scan()` はエンコード後の値をデコードして返すため、デコード→再エンコードが
/// 恒等写像でない将来の変更を見逃し得る。TABLE-4/TABLE-5 の検証は、この生バイト列
/// 比較でのみ厳密に行える。
fn read_raw_rows(path: &std::path::Path) -> Vec<((String, u64), Vec<u8>)> {
    let db = redb::Database::open(path).expect("reopen raw database for row inspection");
    let read_txn = db.begin_read().expect("begin_read");
    let table = read_txn.open_table(ROWS_TABLE).expect("open rows table");
    table
        .iter()
        .expect("iterate rows table")
        .map(|entry| {
            let (k, v) = entry.expect("row entry");
            let (tenant_id, id) = k.value();
            ((tenant_id.to_string(), id), v.value().to_vec())
        })
        .collect()
}

#[test]
fn table4_create_table_does_not_touch_row_data() {
    let path = unique_db_path("table4-rows-untouched");
    let _guard = CleanupGuard(path.clone());

    {
        let storage = Storage::open(&path).expect("open storage");
        seed_rows(&storage, 500);
    } // Storage を drop し、raw ハンドルでの読み出しに備えてファイルロックを解放する。
    let before = read_raw_rows(&path);

    {
        let storage = Storage::open(&path).expect("reopen storage");
        storage
            .create_table(&embedding_schema("docs", 8))
            .expect("create_table");
    }
    let after = read_raw_rows(&path);

    assert_eq!(
        before, after,
        "create_table must not modify row bytes (raw byte comparison)"
    );
}

// `create_table` の所要時間が行数に依存しないこと（TABLE-4）の時間判定は、fsync の多い
// 本ファイルの兄弟テストとの並走を構造的に断つため `tests/catalog_latency.rs` へ移設した
// （Issue #1164）。

// --- TABLE-5 -------------------------------------------------------------

#[test]
fn table5_alter_table_add_column_preserves_existing_row_bytes() {
    let path = unique_db_path("table5-rows-preserved");
    let _guard = CleanupGuard(path.clone());

    {
        let storage = Storage::open(&path).expect("open storage");
        seed_rows(&storage, 500);
        storage
            .create_table(&embedding_schema("docs", 8))
            .expect("create_table");
    } // Storage を drop し、raw ハンドルでの読み出しに備えてファイルロックを解放する。
    let before = read_raw_rows(&path);

    let schema = {
        let storage = Storage::open(&path).expect("reopen storage");
        storage
            .alter_table_add_column("docs", ColumnDef::new("tag", ColumnType::Text, true))
            .expect("alter_table_add_column");
        storage.get_table_schema("docs").expect("get_table_schema")
    };
    let after = read_raw_rows(&path);

    assert_eq!(
        before, after,
        "alter_table_add_column must not rewrite rows (raw byte comparison)"
    );

    let added = schema
        .columns
        .iter()
        .find(|c| c.name == "tag")
        .expect("new column present");
    assert!(added.nullable, "columns added via ALTER must be nullable");
    assert_eq!(added.ty, ColumnType::Text);
}

#[test]
fn table5_alter_table_add_column_rejects_not_null_without_default_regardless_of_rows() {
    // `DEFAULT` なしの `NOT NULL` は行の有無にかかわらず一律 `Invalid` で拒否する
    // （TABLE-16。行の有無で成否が変わると他テナントの行の存在が漏れるため）。
    let path = unique_db_path("table5-not-null-uniform");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&embedding_schema("docs", 8))
        .expect("create_table");

    let err = storage
        .alter_table_add_column("docs", ColumnDef::new("tag", ColumnType::Text, false))
        .expect_err("NOT NULL without DEFAULT must be rejected");
    assert!(matches!(err, CatalogError::Invalid(_)), "got {err:?}");
    let schema = storage.get_table_schema("docs").expect("get_table_schema");
    assert!(schema.columns.iter().all(|c| c.name != "tag"));
}

#[test]
fn table5_alter_table_add_column_rejects_unbindable_default() {
    // 束縛できない DEFAULT はカタログへ永続化しない（既存行の読み出しが常に
    // 失敗する状態を作らない。Issue #1169）。
    let path = unique_db_path("table5-unbindable-default");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&embedding_schema("docs", 8))
        .expect("create_table");

    let bad = ColumnDef::new("n", ColumnType::Integer, true)
        .with_default(engine::catalog::ColumnDefault::Number("abc".to_string()));
    let result = storage.alter_table_add_column("docs", bad);
    assert!(
        matches!(result, Err(CatalogError::Invalid(_))),
        "expected Invalid, got {result:?}"
    );
    let schema = storage.get_table_schema("docs").expect("get_table_schema");
    assert!(!schema.columns.iter().any(|c| c.name == "n"));
}

#[test]
fn table5_alter_table_add_column_with_default_preserves_existing_row_bytes() {
    // DEFAULT 付きでも既存行のバイト列は書き換えない（読み出し時補完。Issue #1169）。
    let path = unique_db_path("table5-default-rows-preserved");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        seed_rows(&storage, 200);
        storage
            .create_table(&embedding_schema("docs", 8))
            .expect("create_table");
    }
    let before = read_raw_rows(&path);
    {
        let storage = Storage::open(&path).expect("reopen storage");
        let col = ColumnDef::new("tag", ColumnType::Text, false)
            .with_default(engine::catalog::ColumnDefault::Text("x".to_string()));
        storage
            .alter_table_add_column("docs", col)
            .expect("alter_table_add_column with DEFAULT");
    }
    assert_eq!(before, read_raw_rows(&path));
}

#[test]
fn table5_alter_table_add_column_latency_is_independent_of_row_count() {
    let small_path = unique_db_path("table5-latency-small");
    let _small_guard = CleanupGuard(small_path.clone());
    let small = Storage::open(&small_path).expect("open small storage");
    seed_rows(&small, 1_000);
    small
        .create_table(&embedding_schema("docs", 8))
        .expect("create_table small");

    let large_path = unique_db_path("table5-latency-large");
    let _large_guard = CleanupGuard(large_path.clone());
    let large = Storage::open(&large_path).expect("open large storage");
    seed_rows(&large, 10_000);
    large
        .create_table(&embedding_schema("docs", 8))
        .expect("create_table large");

    // ウォームアップ 1 回（table4 の latency テスト・`tests/incremental_write_perf.rs`
    // と同じ方針。計測対象の外側で初回コストを吸収する）。
    small
        .alter_table_add_column(
            "docs",
            ColumnDef::new("col_s_warmup", ColumnType::Text, true),
        )
        .expect("warmup alter small");
    large
        .alter_table_add_column(
            "docs",
            ColumnDef::new("col_l_warmup", ColumnType::Text, true),
        )
        .expect("warmup alter large");

    const ROUNDS: usize = 7;
    let mut small_durations = Vec::with_capacity(ROUNDS);
    let mut large_durations = Vec::with_capacity(ROUNDS);

    for i in 0..ROUNDS {
        let start = Instant::now();
        small
            .alter_table_add_column(
                "docs",
                ColumnDef::new(format!("col_s{i}"), ColumnType::Text, true),
            )
            .expect("alter small");
        small_durations.push(start.elapsed());

        let start = Instant::now();
        large
            .alter_table_add_column(
                "docs",
                ColumnDef::new(format!("col_l{i}"), ColumnType::Text, true),
            )
            .expect("alter large");
        large_durations.push(start.elapsed());
    }

    small_durations.sort();
    large_durations.sort();
    let median_small = small_durations[ROUNDS / 2];
    let median_large = large_durations[ROUNDS / 2];

    let ratio = median_large.as_secs_f64().max(1e-9) / median_small.as_secs_f64().max(1e-9);
    assert!(
        ratio < 5.0,
        "alter_table_add_column median latency scaled with row count too much: small={median_small:?}, large={median_large:?}, ratio={ratio}"
    );
}

// --- TABLE-6 -------------------------------------------------------------

#[test]
fn table6_create_table_rejects_invalid_identifiers() {
    let path = unique_db_path("table6-invalid-identifiers");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let invalid_names = ["1abc", "-abc", "a b", "", "a:b", "a\nb", "héllo"];
    for name in invalid_names {
        let schema = embedding_schema(name, 8);
        let result = storage.create_table(&schema);
        assert!(
            matches!(result, Err(CatalogError::Invalid(_))),
            "expected Err for identifier {name:?}, got {result:?}"
        );
    }

    let long_name = "a".repeat(64);
    let schema = embedding_schema(&long_name, 8);
    assert!(matches!(
        storage.create_table(&schema),
        Err(CatalogError::Invalid(_))
    ));
}

#[test]
fn table6_create_table_rejects_invalid_vector_dimension() {
    let path = unique_db_path("table6-invalid-dim");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    assert!(matches!(
        storage.create_table(&embedding_schema("docs_zero", 0)),
        Err(CatalogError::Invalid(_))
    ));
    assert!(matches!(
        storage.create_table(&embedding_schema("docs_huge", 65_537)),
        Err(CatalogError::Invalid(_))
    ));
}

#[test]
fn table6_create_table_rejects_duplicate_and_existing_table() {
    let path = unique_db_path("table6-duplicate");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let dup_schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(8), false),
            ColumnDef::new("embedding", ColumnType::Text, false),
        ],
    );
    assert!(matches!(
        storage.create_table(&dup_schema),
        Err(CatalogError::Invalid(_))
    ));

    storage
        .create_table(&embedding_schema("docs", 8))
        .expect("first create_table");
    assert!(matches!(
        storage.create_table(&embedding_schema("docs", 8)),
        Err(CatalogError::TableAlreadyExists(_))
    ));
}

#[test]
fn table6_alter_table_rejects_missing_table_and_duplicate_column() {
    let path = unique_db_path("table6-alter-errors");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    assert!(matches!(
        storage.alter_table_add_column("missing", ColumnDef::new("tag", ColumnType::Text, true)),
        Err(CatalogError::TableNotFound(_))
    ));

    storage
        .create_table(&embedding_schema("docs", 8))
        .expect("create_table");
    assert!(matches!(
        storage.alter_table_add_column("docs", ColumnDef::new("embedding", ColumnType::Text, true)),
        Err(CatalogError::ColumnAlreadyExists(_))
    ));
}

#[test]
fn table6_decode_rejects_hand_crafted_invalid_catalog_bytes() {
    // 手作りの不正カタログバイト列を redb に直接書き込み、読み出しが panic せず
    // `Err` になることを検証する（欠落フィールド・未知型名・未知バージョン・
    // 不正 UTF-8・不正次元・切り詰め）。
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("missing-cols-line", b"v2\n".to_vec()),
        ("unknown-version", b"v99\ncols:0\n".to_vec()),
        // カタログ v1（Issue #880 で v2 へ更新される前の正当な形式）はマイグレー
        // ションを提供せず fail-closed に拒否する（D6）。
        ("legacy-v1", b"v1\ncols:1\nfoo:text:-:0\n".to_vec()),
        ("unknown-type", b"v2\ncols:1\nfoo:blob:-:0\n".to_vec()),
        ("bad-dimension", b"v2\ncols:1\nfoo:vector:0:0\n".to_vec()),
        ("truncated-columns", b"v2\ncols:2\nfoo:text:-:0\n".to_vec()),
        ("invalid-utf8", vec![0xff, 0xfe, 0xfd]),
        ("missing-field", b"v2\ncols:1\nfoo:text:-\n".to_vec()),
    ];

    // catalog.rs 内部と同一のテーブル定義（キー・値の型とテーブル名 "catalog"）を
    // テスト側で再宣言する。`redb::TableDefinition` はテーブル名という文字列のみで
    // 同一性が決まるため、raw ハンドルからでも catalog.rs が使うテーブルと
    // 同じ実体を開ける（catalog.rs のエンコード形式には依存しない、生のバイト列注入
    // という観点のテスト。tests/persistence.rs と同じ「Storage を drop してファイル
    // ロックを解放してから raw ハンドルで再オープンする」流儀を踏襲する）。
    const RAW_CATALOG_TABLE: redb::TableDefinition<&str, &[u8]> =
        redb::TableDefinition::new("catalog");

    for (label, bytes) in cases {
        let path = unique_db_path(&format!("table6-decode-{label}"));
        let _guard = CleanupGuard(path.clone());

        {
            let storage = Storage::open(&path).expect("open storage");
            // カタログテーブルを実在させるため、先に正当な CREATE TABLE を 1 件通す。
            storage
                .create_table(&embedding_schema("seed", 8))
                .expect("seed create_table");
        } // ここで Storage（= redb::Database）を drop し、ファイルロックを解放する。

        {
            let db = redb::Database::open(&path).expect("reopen raw database");
            let write_txn = db.begin_write().expect("begin_write");
            {
                let mut table = write_txn.open_table(RAW_CATALOG_TABLE).expect("open_table");
                table
                    .insert("broken", bytes.as_slice())
                    .expect("insert broken bytes");
            }
            write_txn.commit().expect("commit");
        }

        let storage = Storage::open(&path).expect("reopen storage");
        let result = storage.get_table_schema("broken");
        // 格納済みバイト列のデコード失敗は `CatalogError::CorruptSchema`（`Invalid`
        // とは区別。Issue #55 レビュー指摘: 前者はユーザー入力の識別子形式不正、
        // 後者はストレージ破損で detail に生データ断片を含み得るため wire_code の
        // 割り当てを分ける）。
        assert!(
            matches!(result, Err(CatalogError::CorruptSchema(_))),
            "case {label}: expected Err(CorruptSchema), got {result:?}"
        );
    }
}

// --- drop_table（Issue #179。PR #151 レビュー据え置き事項の公開 API 契約） -------

#[test]
fn drop_table_rejects_missing_table_name() {
    let path = unique_db_path("drop-table-missing");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let result = storage.drop_table("does_not_exist");
    assert!(
        matches!(result, Err(CatalogError::TableNotFound(_))),
        "got {result:?}"
    );
}

#[test]
fn drop_table_rejects_invalid_identifier() {
    let path = unique_db_path("drop-table-invalid-identifier");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    assert!(matches!(
        storage.drop_table("bad name"),
        Err(CatalogError::Invalid(_))
    ));
    assert!(matches!(
        storage.drop_table(""),
        Err(CatalogError::Invalid(_))
    ));
}

// テーブルを drop してから同名・別次元で再作成すると、旧行テーブル（`user_rows/{table}`）
// が残留せず、新しい次元の行だけが挿入・走査できる（TABLE-4/TABLE-6・EXT-2 の
// 次元固定の不変条件が drop・再作成をまたいでも守られることの確認）。
#[test]
fn drop_table_then_recreate_with_different_dim_has_no_leftover_rows() {
    let path = unique_db_path("drop-table-recreate-different-dim");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let ctx = engine::policy::PolicyContext::new("tenant-a").expect("valid tenant");
    let op_id = engine::recovery::required_op_id::OperationId::parse("drop-table-test")
        .expect("valid operation_id");

    storage
        .create_table(&embedding_schema("docs", 4))
        .expect("create_table");
    engine::tenant::insert_row(
        &storage,
        "docs",
        &ctx,
        1,
        &row(&[1.0, 0.0, 0.0, 0.0], b"old"),
        &op_id,
    )
    .expect("insert row before drop");

    storage.drop_table("docs").expect("drop_table");

    // 再作成前はカタログにも存在しない。
    assert!(matches!(
        storage.get_table_schema("docs"),
        Err(CatalogError::TableNotFound(_))
    ));

    let new_schema = embedding_schema("docs", 8);
    storage
        .create_table(&new_schema)
        .expect("recreate with different dim");

    // 旧次元（4）の埋め込みはもう受理されない（新スキーマが優先される）。
    assert!(engine::tenant::insert_row(
        &storage,
        "docs",
        &ctx,
        2,
        &row(&[1.0, 0.0, 0.0, 0.0], b"stale-dim"),
        &op_id
    )
    .is_err());

    engine::tenant::insert_row(
        &storage,
        "docs",
        &ctx,
        3,
        &row(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], b"new"),
        &op_id,
    )
    .expect("insert row with new dim");

    let (rows, cursor) = storage
        .scan_table_page("docs", None, 100)
        .expect("scan_table_page after recreate");
    assert!(cursor.is_none());
    assert_eq!(
        rows.len(),
        1,
        "only the row inserted after recreate must be present (no leftover from before drop)"
    );
    assert_eq!(rows[0].id, 3);
}
