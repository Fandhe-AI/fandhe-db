//! `engine::catalog` の型横断カタログ回帰テスト（Issue #897、対象ビヘイビア:
//! TABLE-6・TABLE-7・TASK-86。ポインタ: `docs/spec/04-behavior/data-model.md`）。
//!
//! `tests/catalog.rs`（TASK-85・TABLE-6 の基礎テスト）と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、raw `redb::Database` への直接書き込みで
//! 手作りの不正カタログバイト列を注入する）を踏襲し、全 16 種の `ColumnType`
//! について次を横断的に固定する。
//!
//! - 全型を持つテーブルのカタログ往復（再オープンを含む）
//! - 型ごとの `param` 文法違反の拒否
//! - カタログのバージョン行・`enum_types` blob のバージョン不一致の拒否
//!   （受け入れ条件 4）
//! - `Storage` 経由の typed 行往復（`tenant::insert_typed_row` →
//!   `get_row_from_table` → `row_codec::decode_scalar_columns`）

use engine::catalog::{ArrayElemType, ArrayType, CatalogError, ColumnDef, ColumnType};
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{decode_scalar_columns, encode_scalar_columns, ArrayValue, Value};
use engine::storage::{Storage, Visibility};
use engine::uuid::Uuid;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// catalog.rs 内部と同一のテーブル定義。テスト側で raw ハンドルから同じ実体を
// 開くための再宣言（tests/catalog.rs と同じ流儀）。
const RAW_CATALOG_TABLE: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("catalog");
const RAW_ENUM_TYPES_TABLE: redb::TableDefinition<&str, &[u8]> =
    redb::TableDefinition::new("enum_types");

const TABLE: &str = "docs";
const ENUM_TYPE: &str = "mood";

fn enum_labels() -> Vec<String> {
    vec!["alpha".to_string(), "beta".to_string(), "omega".to_string()]
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 全 16 型を持つテーブルスキーマ（`ColumnType::Enum` を含むため `Storage` から
/// 取得した `Arc<EnumTypeDef>` を渡す。手組みの `TableSchema` は使わない）。
fn full_schema(
    enum_def: std::sync::Arc<engine::catalog::EnumTypeDef>,
) -> engine::catalog::TableSchema {
    engine::catalog::TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("c_text", ColumnType::Text, true),
            ColumnDef::new("c_vector", ColumnType::Vector(4), false),
            ColumnDef::new("c_integer", ColumnType::Integer, true),
            ColumnDef::new("c_bigint", ColumnType::BigInt, true),
            ColumnDef::new("c_real", ColumnType::Real, true),
            ColumnDef::new("c_double", ColumnType::Double, true),
            ColumnDef::new("c_boolean", ColumnType::Boolean, true),
            ColumnDef::new("c_date", ColumnType::Date, true),
            ColumnDef::new("c_timestamp", ColumnType::Timestamp, true),
            ColumnDef::new(
                "c_array_text",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
            ColumnDef::new(
                "c_array_bool",
                ColumnType::Array(ArrayType::new(ArrayElemType::Bool, 4).expect("array ty")),
                true,
            ),
            ColumnDef::new("c_bytea", ColumnType::Bytea, true),
            ColumnDef::new("c_json", ColumnType::Json, true),
            ColumnDef::new("c_jsonb", ColumnType::Jsonb, true),
            ColumnDef::new("c_enum", ColumnType::Enum(enum_def), true),
            ColumnDef::new(
                "c_numeric",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("c_uuid", ColumnType::Uuid, true),
        ],
    )
}

// --- カタログ往復（再オープンを含む）------------------------------------------

#[test]
fn full_schema_roundtrips_through_create_table_and_reopen() {
    let path = unique_db_path("column-type-catalog-roundtrip");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        let def = storage
            .create_enum_type(ENUM_TYPE, enum_labels())
            .expect("create enum type");
        storage
            .create_table(&full_schema(def))
            .expect("create table");
        // ADD COLUMN で nullable 列を追加できることも確認する（TABLE-5）。
        storage
            .alter_table_add_column(TABLE, ColumnDef::new("extra_text", ColumnType::Text, true))
            .expect("alter table add column");
        let fetched = storage.get_table_schema(TABLE).expect("get schema");
        assert_eq!(fetched.columns.len(), 18);
        assert_eq!(fetched.columns[17].name, "extra_text");
        assert!(fetched.columns[17].nullable);
    }
    // 再オープン後も往復する。
    {
        let storage = Storage::open(&path).expect("reopen storage");
        let fetched = storage
            .get_table_schema(TABLE)
            .expect("get schema after reopen");
        assert_eq!(fetched.columns.len(), 18);
        assert_eq!(fetched.columns[0].ty, ColumnType::Text);
        assert_eq!(fetched.columns[1].ty, ColumnType::Vector(4));
        assert_eq!(fetched.columns[2].ty, ColumnType::Integer);
        assert_eq!(fetched.columns[3].ty, ColumnType::BigInt);
        assert_eq!(fetched.columns[4].ty, ColumnType::Real);
        assert_eq!(fetched.columns[5].ty, ColumnType::Double);
        assert_eq!(fetched.columns[6].ty, ColumnType::Boolean);
        assert_eq!(fetched.columns[7].ty, ColumnType::Date);
        assert_eq!(fetched.columns[8].ty, ColumnType::Timestamp);
        assert_eq!(
            fetched.columns[9].ty,
            ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).unwrap())
        );
        assert_eq!(
            fetched.columns[10].ty,
            ColumnType::Array(ArrayType::new(ArrayElemType::Bool, 4).unwrap())
        );
        assert_eq!(fetched.columns[11].ty, ColumnType::Bytea);
        assert_eq!(fetched.columns[12].ty, ColumnType::Json);
        assert_eq!(fetched.columns[13].ty, ColumnType::Jsonb);
        match &fetched.columns[14].ty {
            ColumnType::Enum(def) => {
                assert_eq!(def.name(), ENUM_TYPE);
                assert_eq!(def.labels(), enum_labels().as_slice());
            }
            other => panic!("expected ColumnType::Enum, got {other:?}"),
        }
        assert_eq!(
            fetched.columns[15].ty,
            ColumnType::Numeric {
                precision: 10,
                scale: 2
            }
        );
        assert_eq!(fetched.columns[16].ty, ColumnType::Uuid);
    }
}

#[test]
fn numeric_and_array_boundary_params_roundtrip() {
    let path = unique_db_path("column-type-catalog-numeric-array-boundary");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let schema = engine::catalog::TableSchema::new(
        "boundary",
        vec![
            ColumnDef::new(
                "n_min",
                ColumnType::Numeric {
                    precision: 1,
                    scale: 0,
                },
                true,
            ),
            ColumnDef::new(
                "n_max",
                ColumnType::Numeric {
                    precision: 38,
                    scale: 38,
                },
                true,
            ),
            ColumnDef::new(
                "a_min",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 1).expect("array ty")),
                true,
            ),
            ColumnDef::new(
                "a_max",
                ColumnType::Array(
                    ArrayType::new(ArrayElemType::Bool, engine::catalog::MAX_ARRAY_ELEMENTS)
                        .expect("array ty"),
                ),
                true,
            ),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let fetched = storage.get_table_schema("boundary").expect("get schema");
    assert_eq!(
        fetched.columns[0].ty,
        ColumnType::Numeric {
            precision: 1,
            scale: 0
        }
    );
    assert_eq!(
        fetched.columns[1].ty,
        ColumnType::Numeric {
            precision: 38,
            scale: 38
        }
    );
    assert_eq!(
        fetched.columns[2].ty,
        ColumnType::Array(ArrayType::new(ArrayElemType::Text, 1).unwrap())
    );
    assert_eq!(
        fetched.columns[3].ty,
        ColumnType::Array(
            ArrayType::new(ArrayElemType::Bool, engine::catalog::MAX_ARRAY_ELEMENTS).unwrap()
        )
    );
}

// --- 型ごとの param 文法違反（手作りカタログバイト列） -------------------------

fn seed_broken_catalog_entry(path: &std::path::Path, line: &str) {
    {
        let storage = Storage::open(path).expect("open storage");
        // catalog テーブル自体を実在させるため、先に正当な CREATE TABLE を通す。
        storage
            .create_table(&engine::catalog::TableSchema::new(
                "seed",
                vec![ColumnDef::new("v", ColumnType::Vector(4), false)],
            ))
            .expect("seed create_table");
    }
    let bytes = format!("v2\ncols:1\n{line}\n").into_bytes();
    let db = redb::Database::open(path).expect("reopen raw database");
    let write_txn = db.begin_write().expect("begin_write");
    {
        let mut table = write_txn.open_table(RAW_CATALOG_TABLE).expect("open_table");
        table
            .insert("broken", bytes.as_slice())
            .expect("insert broken bytes");
    }
    write_txn.commit().expect("commit");
}

#[test]
fn decode_rejects_type_specific_param_grammar_violations() {
    // (列行, ラベル) のケース表。いずれも `get_table_schema("broken")` が
    // `Err(CatalogError::CorruptSchema(_))` を返すことを検証する。
    let cases: &[(&str, &str)] = &[
        ("no-param-integer", "foo:integer:5:0"),
        ("no-param-bigint", "foo:bigint:x:0"),
        ("no-param-real", "foo:real:1:0"),
        ("no-param-double", "foo:double:1:0"),
        ("no-param-boolean", "foo:boolean:1:0"),
        ("no-param-date", "foo:date:1:0"),
        ("no-param-timestamp", "foo:timestamp:6:0"),
        ("no-param-bytea", "foo:bytea:1:0"),
        ("no-param-json", "foo:json:1:0"),
        ("no-param-jsonb", "foo:jsonb:1:0"),
        ("no-param-uuid", "foo:uuid:1:0"),
        ("numeric-precision-overflow", "foo:numeric:39,0:0"),
        ("numeric-scale-exceeds-precision", "foo:numeric:5,6:0"),
        ("numeric-zero-precision", "foo:numeric:0,0:0"),
        ("numeric-leading-zero", "foo:numeric:010,2:0"),
        ("numeric-missing-scale", "foo:numeric:10:0"),
        ("array-max-len-zero", "foo:array:text,0:0"),
        ("array-max-len-over-limit", "foo:array:text,1025:0"),
        ("array-unknown-elem-type", "foo:array:int,4:0"),
        ("array-nested-array-not-supported", "foo:array:array,4:0"),
    ];

    for (label, line) in cases {
        let path = unique_db_path(&format!("catalog-param-violation-{label}"));
        let _guard = CleanupGuard(path.clone());
        seed_broken_catalog_entry(&path, line);
        let storage = Storage::open(&path).expect("reopen storage");
        let result = storage.get_table_schema("broken");
        assert!(
            matches!(result, Err(CatalogError::CorruptSchema(_))),
            "case {label} ({line:?}): expected Err(CorruptSchema), got {result:?}"
        );
    }
}

#[test]
fn decode_rejects_enum_column_referencing_unregistered_type() {
    // ENUM 列の `param` は型名そのもの（Issue #890）。カタログに未登録の型名を
    // 直接注入すると、`from_catalog_fields` の `resolve_enum` が
    // `CatalogError::TypeNotFound` を返す（`Invalid` ではないため
    // `CorruptSchema` へは読み替わらない。`decode_schema_with_resolver` の契約）。
    let path = unique_db_path("catalog-enum-unregistered-type");
    let _guard = CleanupGuard(path.clone());
    seed_broken_catalog_entry(&path, "foo:enum:not_registered:0");
    let storage = Storage::open(&path).expect("reopen storage");
    let result = storage.get_table_schema("broken");
    assert!(
        matches!(result, Err(CatalogError::TypeNotFound(_))),
        "expected Err(TypeNotFound), got {result:?}"
    );
}

// --- 受け入れ条件 4: カタログ・enum_types blob のバージョン不一致 -------------

#[test]
fn decode_rejects_catalog_version_line_variants() {
    let cases: &[(&str, &[u8])] = &[
        ("v99", b"v99\ncols:0\n"),
        ("v3", b"v3\ncols:0\n"),
        ("uppercase-v2", b"V2\ncols:0\n"),
        ("empty-version-line", b"\ncols:0\n"),
        ("trailing-space", b"v2 \ncols:0\n"),
        ("trailing-cr", b"v2\r\ncols:0\n"),
    ];
    for (label, bytes) in cases {
        let path = unique_db_path(&format!("catalog-version-line-{label}"));
        let _guard = CleanupGuard(path.clone());
        {
            let storage = Storage::open(&path).expect("open storage");
            storage
                .create_table(&engine::catalog::TableSchema::new(
                    "seed",
                    vec![ColumnDef::new("v", ColumnType::Vector(4), false)],
                ))
                .expect("seed create_table");
        }
        {
            let db = redb::Database::open(&path).expect("reopen raw database");
            let write_txn = db.begin_write().expect("begin_write");
            {
                let mut table = write_txn.open_table(RAW_CATALOG_TABLE).expect("open_table");
                table.insert("broken", *bytes).expect("insert broken bytes");
            }
            write_txn.commit().expect("commit");
        }
        let storage = Storage::open(&path).expect("reopen storage");
        let result = storage.get_table_schema("broken");
        assert!(
            matches!(result, Err(CatalogError::CorruptSchema(_))),
            "case {label}: expected Err(CorruptSchema), got {result:?}"
        );
    }
}

/// ENUM 型を作成・テーブルへ結線したうえで、`enum_types` blob を raw ハンドルで
/// 直接書き換える共通セットアップ。
fn seed_enum_type_and_corrupt_blob(path: &std::path::Path, corrupted: &[u8]) {
    {
        let storage = Storage::open(path).expect("open storage");
        let def = storage
            .create_enum_type(ENUM_TYPE, enum_labels())
            .expect("create enum type");
        storage
            .create_table(&engine::catalog::TableSchema::new(
                TABLE,
                vec![
                    ColumnDef::new("v", ColumnType::Vector(4), false),
                    ColumnDef::new("mood", ColumnType::Enum(def), true),
                ],
            ))
            .expect("create table referencing enum");
    }
    let db = redb::Database::open(path).expect("reopen raw database");
    let write_txn = db.begin_write().expect("begin_write");
    {
        let mut table = write_txn
            .open_table(RAW_ENUM_TYPES_TABLE)
            .expect("open enum_types table");
        table
            .insert(ENUM_TYPE, corrupted)
            .expect("insert corrupted enum blob");
    }
    write_txn.commit().expect("commit");
}

#[test]
fn decode_rejects_enum_types_blob_version_mismatch() {
    let cases: &[(&str, &[u8])] = &[
        (
            "version-0",
            &[0x00, 0x01, 0x00, 0x05, b'a', b'l', b'p', b'h', b'a'],
        ),
        (
            "version-2",
            &[0x02, 0x01, 0x00, 0x05, b'a', b'l', b'p', b'h', b'a'],
        ),
        ("empty-blob", &[]),
        ("declared-count-zero", &[0x01, 0x00, 0x00]),
        ("truncated-mid-label", &[0x01, 0x01, 0x00, 0x05, b'a', b'l']),
        (
            "trailing-surplus-bytes",
            &[0x01, 0x01, 0x00, 0x01, b'a', 0xff],
        ),
    ];
    for (label, bytes) in cases {
        let path = unique_db_path(&format!("enum-types-blob-{label}"));
        let _guard = CleanupGuard(path.clone());
        seed_enum_type_and_corrupt_blob(&path, bytes);

        let storage = Storage::open(&path).expect("reopen storage");
        // get_table_schema 経由（テーブルの ENUM 列を解決する過程で参照される）。
        let table_result = storage.get_table_schema(TABLE);
        assert!(
            matches!(table_result, Err(CatalogError::CorruptSchema(_))),
            "case {label}: get_table_schema expected Err(CorruptSchema), got {table_result:?}"
        );
        // get_enum_type 経由（直接参照）でも同じく fail-closed。
        let enum_result = storage.get_enum_type(ENUM_TYPE);
        assert!(
            matches!(enum_result, Err(CatalogError::CorruptSchema(_))),
            "case {label}: get_enum_type expected Err(CorruptSchema), got {enum_result:?}"
        );
    }
}

// --- Storage 経由の typed 往復（`tenant::insert_typed_row`） ------------------

#[test]
fn typed_row_roundtrips_boundary_values_across_reopen() {
    let path = unique_db_path("column-type-catalog-typed-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");
    let schema_columns_len;
    let values = vec![
        Value::Text(String::new()),
        Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
        Value::Integer(i32::MIN),
        Value::BigInt(i64::MAX),
        Value::Real(f32::MAX),
        Value::Double(-f64::MAX),
        Value::Bool(true),
        Value::Date(engine::datetime::DATE_MIN_DAYS),
        Value::Timestamp(engine::datetime::TIMESTAMP_MAX_MICROS),
        Value::Array(ArrayValue::Text(vec![Some("x".to_string())])),
        Value::Array(ArrayValue::Bool(vec![Some(false), Some(true)])),
        Value::Bytes(vec![0xff; 8]),
        Value::Json("{\"k\":\"v\"}".to_string()),
        Value::Json("{}".to_string()),
        Value::Enum("omega".to_string()),
        Value::Numeric(Decimal::from_parts(123_456, 2).expect("decimal")),
        Value::Uuid(Uuid::from_bytes([0x42; 16])),
    ];
    {
        let storage = Storage::open(&path).expect("open storage");
        let def = storage
            .create_enum_type(ENUM_TYPE, enum_labels())
            .expect("create enum type");
        let schema = full_schema(def);
        schema_columns_len = schema.columns.len();
        storage.create_table(&schema).expect("create table");
        let op_id = OperationId::parse("typed-op-1").expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &alice,
            1,
            Visibility::Public,
            &values,
            &op_id,
        )
        .expect("typed insert should succeed");
    }

    // 再オープン後、生の格納済みメタデータを取得し decode_scalar_columns で
    // 検証する（Storage::get_row_from_table は認可を行わない生の取得 API。
    // RLS-9: テストは自テナントの読み戻しにのみ使う）。
    let storage = Storage::open(&path).expect("reopen storage");
    let def = storage.get_enum_type(ENUM_TYPE).expect("get enum type");
    let schema = full_schema(def);
    assert_eq!(schema.columns.len(), schema_columns_len);
    let row = storage
        .get_row_from_table(TABLE, "alice", 1)
        .expect("get row from table");
    let decoded = decode_scalar_columns(&schema, &row.metadata).expect("decode scalar columns");
    // VECTOR 列（index 1）は scalar payload に含まれず Null になる
    // （TABLE-7・row_codec の production 契約）。
    let mut expected = values.clone();
    expected[1] = Value::Null;
    assert_eq!(decoded, expected);

    // 格納済みメタデータの再エンコードがビット同一であることを確認する。
    let re_encoded = encode_scalar_columns(&schema, &decoded).expect("re-encode");
    assert_eq!(re_encoded, row.metadata);
}
