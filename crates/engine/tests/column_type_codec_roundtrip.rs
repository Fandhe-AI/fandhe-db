//! `engine::row_codec` の型横断回帰テスト（Issue #897、対象ビヘイビア: TABLE-7・
//! TASK-86。ポインタ: `docs/spec/04-behavior/data-model.md`）。
//!
//! `tests/row_codec.rs`（TASK-86 の基礎テスト）・`tests/*_column.rs`（型ごとの
//! SQL 表層結合テスト）とは責務を分け、本ファイルは公開コーデック API
//! （`row_codec::encode_row`／`decode_row`／`encode_scalar_columns`／
//! `decode_scalar_columns`／`scan_scalar_columns`／`validate_scalar_columns`）を
//! 全 16 種の `ColumnType` variant について横断的に固定する。
//!
//! - 受け入れ条件 1（ビット単位往復）: encode→decode→re-encode のバイト列一致。
//! - 受け入れ条件 2（境界値・NULL）: 型ごとの最小値・最大値・NULL の保持。
//! - 受け入れ条件 3（fail-closed 拒否）: 範囲外・不正形の encode・decode 拒否。
//!
//! `ColumnType`／`ArrayElemType` に対するワイルドカード腕なしの網羅 `match`
//! （[`type_label`]／[`array_elem_label`]）を置き、型が追加されたときに本ファイルの
//! コンパイルが失敗する（テスト更新を強制する）ことをそれ自体が担保する。
//! `all_column_type_variants_are_covered_by_case_table` がその集合を検査する。

use std::collections::BTreeSet;

use engine::catalog::{
    ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema, MAX_ENUM_LABELS,
};
use engine::numeric::{Decimal, MAX_PRECISION};
use engine::row_codec::{
    decode_row, decode_scalar_columns, encode_row, encode_scalar_columns, scan_scalar_columns,
    validate_scalar_columns, ArrayValue, RowCodecError, Value,
};
use engine::storage::{Storage, Visibility};
use engine::uuid::Uuid;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// src 側の値と同期させる（いずれも `pub(crate)` で結合テストから直接参照できない。
// TABLE-7）。
const MAX_TEXT_FIELD_LEN: u32 = 4 * 1024 * 1024;

const MAX_BYTEA_FIELD_LEN: u32 = engine::bytea::MAX_BYTEA_FIELD_LEN;
const MAX_ARRAY_ELEMENTS: u32 = engine::catalog::MAX_ARRAY_ELEMENTS;
const DATE_MIN_DAYS: i32 = engine::datetime::DATE_MIN_DAYS;
const DATE_MAX_DAYS: i32 = engine::datetime::DATE_MAX_DAYS;
const TIMESTAMP_MIN_MICROS: i64 = engine::datetime::TIMESTAMP_MIN_MICROS;
const TIMESTAMP_MAX_MICROS: i64 = engine::datetime::TIMESTAMP_MAX_MICROS;

const ENUM_TYPE: &str = "mood";

fn enum_labels() -> Vec<String> {
    vec!["alpha".to_string(), "beta".to_string(), "omega".to_string()]
}

/// ワイルドカード腕を持たない `ColumnType` の網羅 `match`。variant が追加されると
/// コンパイルが失敗し、本ファイル（ケース表）の更新を強制する。
fn type_label(ty: &ColumnType) -> &'static str {
    match ty {
        ColumnType::Text => "text",
        ColumnType::Vector(_) => "vector",
        ColumnType::Integer => "integer",
        ColumnType::BigInt => "bigint",
        ColumnType::Real => "real",
        ColumnType::Double => "double",
        ColumnType::Boolean => "boolean",
        ColumnType::Date => "date",
        ColumnType::Timestamp => "timestamp",
        ColumnType::Array(_) => "array",
        ColumnType::Bytea => "bytea",
        ColumnType::Json => "json",
        ColumnType::Jsonb => "jsonb",
        ColumnType::Enum(_) => "enum",
        ColumnType::Numeric { .. } => "numeric",
        ColumnType::Uuid => "uuid",
    }
}

/// `ArrayElemType` の網羅 `match`（同上の理由）。
fn array_elem_label(elem: &ArrayElemType) -> &'static str {
    match elem {
        ArrayElemType::Text => "text",
        ArrayElemType::Bool => "bool",
        ArrayElemType::Integer => "integer",
        ArrayElemType::BigInt => "bigint",
        ArrayElemType::Real => "real",
        ArrayElemType::Double => "double",
        ArrayElemType::Date => "date",
        ArrayElemType::Timestamp => "timestamp",
        ArrayElemType::Uuid => "uuid",
    }
}

fn all_type_labels() -> BTreeSet<&'static str> {
    BTreeSet::from([
        "text",
        "vector",
        "integer",
        "bigint",
        "real",
        "double",
        "boolean",
        "date",
        "timestamp",
        "array",
        "bytea",
        "json",
        "jsonb",
        "enum",
        "numeric",
        "uuid",
    ])
}

/// `Vector` 列を含む、`encode_row`／`decode_row`（テナント・可視性ヘッダ付きの
/// フルスキーマコーデック）向けの全型スキーマ。すべて nullable にして NULL 往復を
/// 型を問わず一様に検証できるようにする。
fn full_row_schema(enum_def: std::sync::Arc<engine::catalog::EnumTypeDef>) -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("c_text", ColumnType::Text, true),
            ColumnDef::new("c_vector", ColumnType::Vector(4), true),
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
                    precision: 38,
                    scale: 10,
                },
                true,
            ),
            ColumnDef::new("c_uuid", ColumnType::Uuid, true),
        ],
    )
}

/// `encode_scalar_columns`／`decode_scalar_columns` 系（`VECTOR` 列を持たない
/// スカラーペイロード）向けのスキーマ。`VECTOR` 列は当該 API から常にスキップ
/// されるため（production 契約）、意図的に含めない。
fn scalar_schema(enum_def: std::sync::Arc<engine::catalog::EnumTypeDef>) -> TableSchema {
    let cols: Vec<ColumnDef> = full_row_schema(enum_def)
        .columns
        .into_iter()
        .filter(|c| !matches!(c.ty, ColumnType::Vector(_)))
        .collect();
    // テーブル名は full_row_schema と別の値（"docs_scalar"）を使う。`TableSchema`
    // は名前を保持するだけで比較や検証に使わないため、独立した名前で構わない。
    TableSchema::new("docs_scalar", cols)
}

fn open_with_enum() -> (
    Storage,
    std::path::PathBuf,
    std::sync::Arc<engine::catalog::EnumTypeDef>,
) {
    let path = unique_db_path("column-type-codec-roundtrip");
    let storage = Storage::open(&path).expect("open storage");
    let def = storage
        .create_enum_type(ENUM_TYPE, enum_labels())
        .expect("create enum type");
    (storage, path, def)
}

/// [`scalar_schema`] の列順（`VECTOR` 列を含まない）に対応する代表値
/// （NULL 以外）。境界値・代表値をひととおり含む。
fn representative_scalar_values() -> Vec<Value> {
    vec![
        Value::Text("hello".to_string()),
        Value::Integer(42),
        Value::BigInt(-9_000_000_000),
        Value::Real(1.5f32),
        Value::Double(-2.25f64),
        Value::Bool(true),
        Value::Date(0),
        Value::Timestamp(0),
        Value::Array(ArrayValue::Text(vec![
            Some("a".to_string()),
            Some("b".to_string()),
        ])),
        Value::Array(ArrayValue::Bool(vec![Some(true), Some(false)])),
        Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        Value::Json(" {\"b\": 2, \"a\": 1} ".trim().to_string()),
        Value::Json("{\"a\":1,\"b\":2}".to_string()),
        Value::Enum("beta".to_string()),
        // full_row_schema/scalar_schema は c_numeric を NUMERIC(38, 10) で
        // 宣言しているため、scale を列宣言に一致させる。
        Value::Numeric(Decimal::from_parts(1_234_560_000, 10).expect("decimal")),
        Value::Uuid(Uuid::from_bytes([0x11; 16])),
    ]
}

/// [`full_row_schema`] の列順（`c_text, c_vector, c_integer, ...`）に対応する
/// 代表値。[`representative_scalar_values`] の先頭（`c_text`）の直後に
/// `c_vector` を挿入する。
fn full_row_values() -> Vec<Value> {
    let mut scalars = representative_scalar_values();
    let vector_value = Value::Vector(vec![0.1, 0.2, 0.3, 0.4]);
    scalars.insert(1, vector_value);
    scalars
}

// --- 受け入れ条件 1・2: `encode_row`／`decode_row` のビット単位往復 -----------

#[test]
fn all_column_type_variants_are_covered_by_case_table() {
    let (_storage, _path, def) = open_with_enum();
    let _guard = CleanupGuard(_path.clone());
    let schema = full_row_schema(def);
    let covered: BTreeSet<&'static str> =
        schema.columns.iter().map(|c| type_label(&c.ty)).collect();
    assert_eq!(
        covered,
        all_type_labels(),
        "ケース表が ColumnType の全 variant を網羅していない"
    );
    // ArrayElemType も同様に両 variant を網羅していることを確認する。
    let array_labels: BTreeSet<&'static str> = [
        ArrayElemType::Text,
        ArrayElemType::Bool,
        ArrayElemType::Integer,
        ArrayElemType::BigInt,
        ArrayElemType::Real,
        ArrayElemType::Double,
        ArrayElemType::Date,
        ArrayElemType::Timestamp,
        ArrayElemType::Uuid,
    ]
    .iter()
    .map(array_elem_label)
    .collect();
    assert_eq!(
        array_labels,
        BTreeSet::from([
            "text",
            "bool",
            "integer",
            "bigint",
            "real",
            "double",
            "date",
            "timestamp",
            "uuid"
        ])
    );
}

#[test]
fn full_row_roundtrip_is_bit_exact_for_representative_values() {
    let (_storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    let schema = full_row_schema(def);
    let values = full_row_values();

    let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
    let decoded = decode_row(&schema, &encoded).expect("decode");
    assert_eq!(decoded.tenant_id, "tenant-a");
    assert_eq!(decoded.visibility, Visibility::Public);
    assert_eq!(decoded.values, values);

    // encode(decode(bytes)) == bytes（再エンコードの完全一致）。
    let re_encoded = encode_row(
        &schema,
        &decoded.tenant_id,
        decoded.visibility,
        &decoded.values,
    )
    .expect("re-encode");
    assert_eq!(re_encoded, encoded);
}

#[test]
fn full_row_roundtrip_all_null_for_every_nullable_column() {
    // 受け入れ条件 2（NULL）: 全型を一度に検証する。全列 nullable の
    // full_row_schema では NULL は列型を問わず一様に扱われる（fail-closed の
    // 分岐は `column.nullable` のみで、型別の特別扱いをしない）。
    let (_storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    let schema = full_row_schema(def);
    let values = vec![Value::Null; schema.columns.len()];
    let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
    let decoded = decode_row(&schema, &encoded).expect("decode");
    assert_eq!(decoded.values, values);
    let re_encoded = encode_row(
        &schema,
        &decoded.tenant_id,
        decoded.visibility,
        &decoded.values,
    )
    .expect("re-encode");
    assert_eq!(re_encoded, encoded);
}

#[test]
fn null_and_empty_value_encode_differently() {
    // NULL と「空値」（空 TEXT・空 BYTEA・空 ARRAY・false・0）はバイト列上も
    // 区別する。
    let schema = TableSchema::new(
        "t",
        vec![
            ColumnDef::new("t", ColumnType::Text, true),
            ColumnDef::new("b", ColumnType::Bytea, true),
            ColumnDef::new(
                "a",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
            ColumnDef::new("bo", ColumnType::Boolean, true),
            ColumnDef::new("n", ColumnType::Integer, true),
        ],
    );
    let null_values = vec![Value::Null; 5];
    let empty_values = vec![
        Value::Text(String::new()),
        Value::Bytes(Vec::new()),
        Value::Array(ArrayValue::Text(Vec::new())),
        Value::Bool(false),
        Value::Integer(0),
    ];
    let null_encoded =
        encode_row(&schema, "tenant-a", Visibility::Public, &null_values).expect("encode null");
    let empty_encoded =
        encode_row(&schema, "tenant-a", Visibility::Public, &empty_values).expect("encode empty");
    assert_ne!(null_encoded, empty_encoded);
}

#[test]
fn missing_trailing_nullable_column_decodes_as_null_for_any_type() {
    // TABLE-5: ADD COLUMN で追加された nullable な末尾列を持たない既存行は
    // Null として読める。型を問わず成立することを ENUM 列（TABLE-14 の新型）で
    // 固定する。
    let (_storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    let schema = full_row_schema(def);
    let mut values = full_row_values();
    // 末尾（uuid）を欠落させる。
    values.pop();
    let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
    let decoded = decode_row(&schema, &encoded).expect("decode");
    assert_eq!(*decoded.values.last().unwrap(), Value::Null);
}

// --- 型ごとの境界値（受け入れ条件 2）------------------------------------------

fn single_col_schema(ty: ColumnType) -> TableSchema {
    TableSchema::new("t", vec![ColumnDef::new("v", ty, false)])
}

fn roundtrip_single(ty: ColumnType, value: Value) {
    let schema = single_col_schema(ty);
    let encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        std::slice::from_ref(&value),
    )
    .expect("encode");
    let decoded = decode_row(&schema, &encoded).expect("decode");
    assert_eq!(decoded.values, vec![value]);
    let re_encoded = encode_row(
        &schema,
        &decoded.tenant_id,
        decoded.visibility,
        &decoded.values,
    )
    .expect("re-encode");
    assert_eq!(re_encoded, encoded);
}

#[test]
fn integer_roundtrips_min_max_zero_neg1() {
    for v in [i32::MIN, i32::MAX, 0, -1] {
        roundtrip_single(ColumnType::Integer, Value::Integer(v));
    }
}

#[test]
fn bigint_roundtrips_min_max() {
    for v in [i64::MIN, i64::MAX, 0] {
        roundtrip_single(ColumnType::BigInt, Value::BigInt(v));
    }
}

#[test]
fn real_roundtrips_boundaries_and_negzero_normalizes_to_poszero() {
    for v in [f32::MAX, -f32::MAX, f32::MIN_POSITIVE, f32::from_bits(1)] {
        roundtrip_single(ColumnType::Real, Value::Real(v));
    }
    // -0.0 は encode 時に +0.0 へ正規化される（恒等性は保証しない）。
    let schema = single_col_schema(ColumnType::Real);
    let encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Real(-0.0f32)],
    )
    .expect("encode -0.0");
    let decoded = decode_row(&schema, &encoded).expect("decode");
    match decoded.values[0] {
        Value::Real(v) => assert_eq!(v.to_bits(), 0.0f32.to_bits(), "-0.0 は +0.0 に正規化される"),
        ref other => panic!("expected Value::Real, got {other:?}"),
    }
    // +0.0 と -0.0 の encode 結果は一致する。
    let pos_encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Real(0.0f32)],
    )
    .expect("encode +0.0");
    assert_eq!(pos_encoded, encoded);
}

#[test]
fn double_roundtrips_boundaries_and_negzero_normalizes_to_poszero() {
    for v in [f64::MAX, -f64::MAX, f64::MIN_POSITIVE, f64::from_bits(1)] {
        roundtrip_single(ColumnType::Double, Value::Double(v));
    }
    let schema = single_col_schema(ColumnType::Double);
    let encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Double(-0.0f64)],
    )
    .expect("encode -0.0");
    let decoded = decode_row(&schema, &encoded).expect("decode");
    match decoded.values[0] {
        Value::Double(v) => assert_eq!(v.to_bits(), 0.0f64.to_bits()),
        ref other => panic!("expected Value::Double, got {other:?}"),
    }
    let pos_encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Double(0.0f64)],
    )
    .expect("encode +0.0");
    assert_eq!(pos_encoded, encoded);
}

#[test]
fn boolean_roundtrips_true_and_false() {
    roundtrip_single(ColumnType::Boolean, Value::Bool(true));
    roundtrip_single(ColumnType::Boolean, Value::Bool(false));
}

#[test]
fn date_roundtrips_min_and_max_days() {
    roundtrip_single(ColumnType::Date, Value::Date(DATE_MIN_DAYS));
    roundtrip_single(ColumnType::Date, Value::Date(DATE_MAX_DAYS));
}

#[test]
fn timestamp_roundtrips_min_and_max_micros() {
    roundtrip_single(
        ColumnType::Timestamp,
        Value::Timestamp(TIMESTAMP_MIN_MICROS),
    );
    roundtrip_single(
        ColumnType::Timestamp,
        Value::Timestamp(TIMESTAMP_MAX_MICROS),
    );
}

#[test]
fn array_text_roundtrips_empty_and_max_len() {
    let ty = ColumnType::Array(
        ArrayType::new(ArrayElemType::Text, MAX_ARRAY_ELEMENTS).expect("array ty"),
    );
    roundtrip_single(ty.clone(), Value::Array(ArrayValue::Text(Vec::new())));
    let full: Vec<Option<String>> = (0..MAX_ARRAY_ELEMENTS)
        .map(|i| Some(format!("v{i}")))
        .collect();
    roundtrip_single(ty, Value::Array(ArrayValue::Text(full)));
}

#[test]
fn array_bool_roundtrips_empty_and_max_len() {
    let ty = ColumnType::Array(
        ArrayType::new(ArrayElemType::Bool, MAX_ARRAY_ELEMENTS).expect("array ty"),
    );
    roundtrip_single(ty.clone(), Value::Array(ArrayValue::Bool(Vec::new())));
    let full: Vec<Option<bool>> = (0..MAX_ARRAY_ELEMENTS).map(|i| Some(i % 2 == 0)).collect();
    roundtrip_single(ty, Value::Array(ArrayValue::Bool(full)));
}

#[test]
fn bytea_roundtrips_empty_and_max_len() {
    roundtrip_single(ColumnType::Bytea, Value::Bytes(Vec::new()));
    let full = vec![0xabu8; MAX_BYTEA_FIELD_LEN as usize];
    roundtrip_single(ColumnType::Bytea, Value::Bytes(full));
}

#[test]
fn text_roundtrips_empty_and_max_len_via_encode_row() {
    roundtrip_single(ColumnType::Text, Value::Text(String::new()));
    let full = "a".repeat(MAX_TEXT_FIELD_LEN as usize);
    roundtrip_single(ColumnType::Text, Value::Text(full));
}

#[test]
fn json_preserves_input_text_including_whitespace() {
    // Json 列は検証済みの入力テキストをそのまま（空白込みで）保持する。
    let text = " { \"a\" : 1 } ".to_string();
    roundtrip_single(ColumnType::Json, Value::Json(text));
}

#[test]
fn jsonb_roundtrips_nested_canonical_text() {
    let text = "{\"a\":[1,2,{\"b\":true}],\"c\":null}".to_string();
    roundtrip_single(ColumnType::Jsonb, Value::Json(text));
}

#[test]
fn json_and_jsonb_empty_object_roundtrips() {
    roundtrip_single(ColumnType::Json, Value::Json("{}".to_string()));
    roundtrip_single(ColumnType::Jsonb, Value::Json("{}".to_string()));
}

#[test]
fn enum_roundtrips_first_and_last_label() {
    let (storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    drop(storage);
    let labels = def.labels().to_vec();
    let schema = single_col_schema(ColumnType::Enum(def));
    roundtrip_single(
        schema.columns[0].ty.clone(),
        Value::Enum(labels.first().unwrap().clone()),
    );
    roundtrip_single(
        schema.columns[0].ty.clone(),
        Value::Enum(labels.last().unwrap().clone()),
    );
}

#[test]
fn numeric_roundtrips_boundaries() {
    let max38 = Decimal::from_parts(10i128.pow(38) - 1, 0).expect("decimal");
    let min38 = Decimal::from_parts(-(10i128.pow(38) - 1), 0).expect("decimal");
    roundtrip_single(
        ColumnType::Numeric {
            precision: 38,
            scale: 0,
        },
        Value::Numeric(max38),
    );
    roundtrip_single(
        ColumnType::Numeric {
            precision: 38,
            scale: 0,
        },
        Value::Numeric(min38),
    );
    // (38, 38) 端点。
    let frac = Decimal::from_parts(10i128.pow(38) - 1, 38).expect("decimal");
    roundtrip_single(
        ColumnType::Numeric {
            precision: 38,
            scale: 38,
        },
        Value::Numeric(frac),
    );
    // (1, 0) 端点。
    let small = Decimal::from_parts(9, 0).expect("decimal");
    roundtrip_single(
        ColumnType::Numeric {
            precision: 1,
            scale: 0,
        },
        Value::Numeric(small),
    );
}

#[test]
fn uuid_roundtrips_nil_and_all_ones() {
    roundtrip_single(ColumnType::Uuid, Value::Uuid(Uuid::from_bytes([0x00; 16])));
    roundtrip_single(ColumnType::Uuid, Value::Uuid(Uuid::from_bytes([0xff; 16])));
}

#[test]
fn vector_roundtrips_dim1_and_declared_dim() {
    roundtrip_single(ColumnType::Vector(1), Value::Vector(vec![1.0]));
    roundtrip_single(
        ColumnType::Vector(4),
        Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
    );
}

// --- `encode_scalar_columns`／`decode_scalar_columns`（VECTOR 列を除く）------

#[test]
fn scalar_columns_roundtrip_is_bit_exact_for_representative_values() {
    let (_storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    let schema = scalar_schema(def);
    let values = representative_scalar_values();

    let encoded = encode_scalar_columns(&schema, &values).expect("encode scalar columns");
    let decoded = decode_scalar_columns(&schema, &encoded).expect("decode scalar columns");
    assert_eq!(decoded, values);

    let re_encoded = encode_scalar_columns(&schema, &decoded).expect("re-encode");
    assert_eq!(re_encoded, encoded);

    // decode_scalar_columns は内部で scan_scalar_columns を呼び出し ScalarRef を
    // Value へ変換するため、直前の assert_eq!(decoded, values) の時点で
    // ScalarRef の値そのものは全列型について既にビット単位で固定済みである
    // （row_codec.rs::decode_scalar_columns 参照）。ここでの直接呼び出しは、
    // decode_scalar_columns を経由しない独立した scan_scalar_columns 単体の
    // 呼び出しがエラーにならず、列数（None を含む）が期待どおりであることを
    // 追加で固定する非自明な検査であり、値の再比較ではない。
    let scanned = scan_scalar_columns(&schema, &encoded).expect("scan scalar columns");
    assert_eq!(scanned.len(), values.len());

    validate_scalar_columns(&schema, &encoded).expect("validate scalar columns");
}

#[test]
fn scalar_columns_roundtrip_all_null() {
    let (_storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    let schema = scalar_schema(def);
    let values = vec![Value::Null; schema.columns.len()];
    let encoded = encode_scalar_columns(&schema, &values).expect("encode");
    let decoded = decode_scalar_columns(&schema, &encoded).expect("decode");
    assert_eq!(decoded, values);
}

#[test]
fn scalar_columns_vector_column_decodes_as_null_placeholder() {
    // VECTOR 列は encode_scalar_columns では書き込み自体をスキップするが
    // （storage.rs 側の embedding スロットが担当）、decode 側
    // （scan_scalar_columns_validated）は列インデックスを保つため Vector 列に
    // 対して常に `None`（= Value::Null）を積む。したがって出力長は
    // schema.columns.len() と一致し、Vector 列の位置は Null になる。
    let (_storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    let schema = full_row_schema(def);
    let vector_idx = schema
        .columns
        .iter()
        .position(|c| matches!(c.ty, ColumnType::Vector(_)))
        .expect("schema has a vector column");
    let values = full_row_values();
    let encoded =
        encode_scalar_columns(&schema, &values).expect("encode with vector column present");
    let decoded = decode_scalar_columns(&schema, &encoded).expect("decode");
    assert_eq!(decoded.len(), schema.columns.len());
    assert_eq!(decoded[vector_idx], Value::Null);
    // Vector 以外の列は encode に渡した値をそのまま保持する。
    for (idx, value) in decoded.iter().enumerate() {
        if idx != vector_idx {
            assert_eq!(*value, values[idx], "column idx {idx} should roundtrip");
        }
    }
}

// --- 受け入れ条件 3: encode 側 fail-closed（型不一致・値域） ------------------

#[test]
fn encode_row_rejects_value_variant_mismatch_for_every_type() {
    // 網羅ループ: 各列に「別の型の値」を渡すと必ず Err になる。
    let (_storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    let schema = full_row_schema(def);
    let wrong_value_for = |label: &str| -> Value {
        if label == "text" {
            Value::Integer(1)
        } else {
            Value::Text("wrong-type".to_string())
        }
    };
    for (idx, col) in schema.columns.iter().enumerate() {
        let label = type_label(&col.ty);
        let mut values = vec![Value::Null; schema.columns.len()];
        values[idx] = wrong_value_for(label);
        // 他の列は NULL（nullable）のままにできるため、対象列だけの型不一致を
        // 検証できる。
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(
            matches!(result, Err(RowCodecError::Invalid(_))),
            "column {label} should reject a value of the wrong Value variant, got {result:?}"
        );
    }
}

#[test]
fn encode_row_rejects_null_for_non_nullable_column() {
    let schema = single_col_schema(ColumnType::Integer);
    let result = encode_row(&schema, "tenant-a", Visibility::Public, &[Value::Null]);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn encode_row_rejects_non_finite_real_and_double() {
    for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let schema = single_col_schema(ColumnType::Real);
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &[Value::Real(v)]);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let schema = single_col_schema(ColumnType::Double);
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &[Value::Double(v)]);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }
}

#[test]
fn encode_row_rejects_date_and_timestamp_out_of_range() {
    let date_schema = single_col_schema(ColumnType::Date);
    for v in [DATE_MIN_DAYS - 1, DATE_MAX_DAYS + 1] {
        let result = encode_row(
            &date_schema,
            "tenant-a",
            Visibility::Public,
            &[Value::Date(v)],
        );
        assert!(matches!(result, Err(RowCodecError::Invalid(_))), "day {v}");
    }
    let ts_schema = single_col_schema(ColumnType::Timestamp);
    for v in [TIMESTAMP_MIN_MICROS - 1, TIMESTAMP_MAX_MICROS + 1] {
        let result = encode_row(
            &ts_schema,
            "tenant-a",
            Visibility::Public,
            &[Value::Timestamp(v)],
        );
        assert!(
            matches!(result, Err(RowCodecError::Invalid(_))),
            "micros {v}"
        );
    }
}

#[test]
fn encode_row_rejects_numeric_precision_and_scale_violations() {
    let schema = single_col_schema(ColumnType::Numeric {
        precision: 5,
        scale: 2,
    });
    // 10^38 は precision=5 の範囲を大きく超える。
    let too_big = Decimal::from_parts(10i128.pow(38), 0).expect("decimal");
    let result = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Numeric(too_big)],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));

    // scale 不一致（列は scale=2、値は scale=3）。
    let mismatched_scale = Decimal::from_parts(12345, 3).expect("decimal");
    let result = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Numeric(mismatched_scale)],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));

    // MAX_PRECISION を超える precision 宣言自体は Decimal::from_parts では
    // 検証されないため（scale のみ検証）、encode 側の精度検査を経由させる。
    let over_precision = Decimal::from_parts(10i128.pow(38) - 1, 0).expect("decimal");
    let schema38 = single_col_schema(ColumnType::Numeric {
        precision: MAX_PRECISION,
        scale: 0,
    });
    // これは境界内で成功するはず（回帰時のガードとして残す）。
    encode_row(
        &schema38,
        "tenant-a",
        Visibility::Public,
        &[Value::Numeric(over_precision)],
    )
    .expect("38 digit boundary should still succeed");
}

#[test]
fn encode_row_rejects_bytea_and_text_over_limit() {
    let text_schema = single_col_schema(ColumnType::Text);
    let oversized_text = "a".repeat(MAX_TEXT_FIELD_LEN as usize + 1);
    let result = encode_row(
        &text_schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Text(oversized_text)],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));

    let bytea_schema = single_col_schema(ColumnType::Bytea);
    let oversized_bytes = vec![0u8; MAX_BYTEA_FIELD_LEN as usize + 1];
    let result = encode_row(
        &bytea_schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Bytes(oversized_bytes)],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn encode_row_rejects_array_over_max_len_and_elem_type_mismatch() {
    let ty = ColumnType::Array(
        ArrayType::new(ArrayElemType::Text, MAX_ARRAY_ELEMENTS).expect("array ty"),
    );
    let schema = single_col_schema(ty);
    let over: Vec<Option<String>> = (0..=MAX_ARRAY_ELEMENTS)
        .map(|i| Some(format!("v{i}")))
        .collect();
    let result = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Array(ArrayValue::Text(over))],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));

    // 要素型不一致（TEXT 列に Bool 配列）。
    let result = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Array(ArrayValue::Bool(vec![Some(true)]))],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn encode_row_rejects_enum_label_outside_vocabulary() {
    let (storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    drop(storage);
    let schema = single_col_schema(ColumnType::Enum(def));
    let result = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Enum("not-a-real-label".to_string())],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn encode_row_rejects_json_syntax_error_and_jsonb_non_canonical_text() {
    let json_schema = single_col_schema(ColumnType::Json);
    let result = encode_row(
        &json_schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Json("{not valid json".to_string())],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));

    let jsonb_schema = single_col_schema(ColumnType::Jsonb);
    // 非正規形（余分な空白）は encode 側で拒否される（JSONB は正規化済み
    // テキストのみを受理する契約）。
    let result = encode_row(
        &jsonb_schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Json("{ \"a\": 1 }".to_string())],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn encode_row_rejects_vector_dimension_mismatch() {
    let schema = single_col_schema(ColumnType::Vector(4));
    let result = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Vector(vec![0.1, 0.2, 0.3])],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn encode_row_rejects_more_values_than_schema_columns() {
    let schema = single_col_schema(ColumnType::Integer);
    let result = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Integer(1), Value::Integer(2)],
    );
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

// --- 受け入れ条件 3: decode 側 fail-closed（手作りバイト列） ------------------

#[test]
fn decode_row_rejects_invalid_presence_byte() {
    let schema = single_col_schema(ColumnType::Boolean);
    let mut encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Bool(true)],
    )
    .expect("encode");
    // ヘッダ（バージョン 1 バイト + 可視性 1 バイト + tenant_len 1 バイト +
    // tenant バイト列）の直後が presence バイト。
    let presence_offset = 3 + "tenant-a".len();
    encoded[presence_offset] = 0x02;
    let result = decode_row(&schema, &encoded);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_invalid_bool_value_byte() {
    let schema = single_col_schema(ColumnType::Boolean);
    let mut encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Bool(true)],
    )
    .expect("encode");
    let value_offset = 3 + "tenant-a".len() + 1;
    encoded[value_offset] = 0x02;
    let result = decode_row(&schema, &encoded);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_nan_and_inf_bit_patterns_for_real_and_double() {
    let schema = single_col_schema(ColumnType::Real);
    let mut encoded =
        encode_row(&schema, "tenant-a", Visibility::Public, &[Value::Real(1.0)]).expect("encode");
    let value_offset = 3 + "tenant-a".len() + 1;
    encoded[value_offset..value_offset + 4].copy_from_slice(&f32::NAN.to_le_bytes());
    let result = decode_row(&schema, &encoded);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));

    let schema = single_col_schema(ColumnType::Double);
    let mut encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Double(1.0)],
    )
    .expect("encode");
    encoded[value_offset..value_offset + 8].copy_from_slice(&f64::INFINITY.to_le_bytes());
    let result = decode_row(&schema, &encoded);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_truncated_fixed_width_field_for_non_nullable_column() {
    // Integer（4 バイト固定幅）を presence バイトの直後で切断する。
    let schema = single_col_schema(ColumnType::Integer);
    let encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Integer(1)],
    )
    .expect("encode");
    let value_offset = 3 + "tenant-a".len() + 1;
    let truncated = &encoded[..value_offset + 2];
    let result = decode_row(&schema, truncated);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_length_field_exceeding_remaining_buffer() {
    let schema = single_col_schema(ColumnType::Text);
    let mut encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Text("hi".to_string())],
    )
    .expect("encode");
    let value_offset = 3 + "tenant-a".len() + 1;
    // 長さフィールド（u32 LE）を実バッファよりはるかに大きい値へ書き換える。
    encoded[value_offset..value_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let result = decode_row(&schema, &encoded);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_trailing_surplus_bytes() {
    let schema = single_col_schema(ColumnType::Integer);
    let mut encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Integer(1)],
    )
    .expect("encode");
    encoded.push(0xff);
    let result = decode_row(&schema, &encoded);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_invalid_utf8_in_text_and_enum() {
    let text_schema = single_col_schema(ColumnType::Text);
    let mut encoded = encode_row(
        &text_schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Text("hi".to_string())],
    )
    .expect("encode");
    let value_offset = 3 + "tenant-a".len() + 1;
    // 長さ 2 バイトはそのまま、本体を不正 UTF-8 へ置換する。
    encoded[value_offset + 4] = 0xff;
    encoded[value_offset + 5] = 0xfe;
    let result = decode_row(&text_schema, &encoded);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));

    let (storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    drop(storage);
    let enum_schema = single_col_schema(ColumnType::Enum(def));
    let mut encoded = encode_row(
        &enum_schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Enum("alpha".to_string())],
    )
    .expect("encode");
    let value_offset = 3 + "tenant-a".len() + 1;
    encoded[value_offset + 4] = 0xff;
    let result = decode_row(&enum_schema, &encoded);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_enum_label_not_in_current_vocabulary() {
    // ENUM 列は decode 時に現行語彙（`EnumTypeDef::labels`）との照合を行う
    // （production の decode 経路コメント参照。codex-review P1 指摘対応）。
    // 手作りバイト列で現行語彙に無いラベルを注入すると fail-closed に拒否される。
    let (storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    drop(storage);
    let schema = single_col_schema(ColumnType::Enum(def));
    let mut encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Enum("alpha".to_string())],
    )
    .expect("encode");
    // "alpha" (5 bytes) を同じ長さの未登録ラベル "zzzzz" に置き換える。
    let value_offset = 3 + "tenant-a".len() + 1 + 4;
    encoded[value_offset..value_offset + 5].copy_from_slice(b"zzzzz");
    let result = decode_row(&schema, &encoded);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_numeric_unscaled_exceeding_precision() {
    let schema = single_col_schema(ColumnType::Numeric {
        precision: 1,
        scale: 0,
    });
    let encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Numeric(Decimal::from_parts(9, 0).expect("decimal"))],
    )
    .expect("encode boundary value");
    let value_offset = 3 + "tenant-a".len() + 1;
    let mut mutated = encoded.clone();
    // unscaled（i128 LE 16 バイト）を precision=1 の範囲外（99）へ書き換える。
    mutated[value_offset..value_offset + 16].copy_from_slice(&99i128.to_le_bytes());
    let result = decode_row(&schema, &mutated);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_date_and_timestamp_value_out_of_range() {
    let schema = single_col_schema(ColumnType::Date);
    let encoded =
        encode_row(&schema, "tenant-a", Visibility::Public, &[Value::Date(0)]).expect("encode");
    let value_offset = 3 + "tenant-a".len() + 1;
    let mut mutated = encoded.clone();
    mutated[value_offset..value_offset + 4].copy_from_slice(&(DATE_MAX_DAYS + 1).to_le_bytes());
    let result = decode_row(&schema, &mutated);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));

    let ts_schema = single_col_schema(ColumnType::Timestamp);
    let ts_encoded = encode_row(
        &ts_schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Timestamp(0)],
    )
    .expect("encode");
    let mut ts_mutated = ts_encoded;
    ts_mutated[value_offset..value_offset + 8]
        .copy_from_slice(&(TIMESTAMP_MAX_MICROS + 1).to_le_bytes());
    let ts_result = decode_row(&ts_schema, &ts_mutated);
    assert!(matches!(ts_result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn decode_row_rejects_array_element_count_exceeding_limit() {
    let ty = ColumnType::Array(
        ArrayType::new(ArrayElemType::Bool, MAX_ARRAY_ELEMENTS).expect("array ty"),
    );
    let schema = single_col_schema(ty);
    let encoded = encode_row(
        &schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Array(ArrayValue::Bool(vec![Some(true)]))],
    )
    .expect("encode single-element array");
    let value_offset = 3 + "tenant-a".len() + 1;
    // 要素数フィールド（u32 LE）を上限超過値へ書き換える。
    let mut mutated = encoded;
    mutated[value_offset..value_offset + 4]
        .copy_from_slice(&(MAX_ARRAY_ELEMENTS + 1).to_le_bytes());
    let result = decode_row(&schema, &mutated);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

// --- decode は panic せず Err を返す（fail-closed の総括確認）----------------

#[test]
fn decode_row_never_panics_on_arbitrary_short_buffers() {
    let (_storage, path, def) = open_with_enum();
    let _guard = CleanupGuard(path);
    let schema = full_row_schema(def);
    for len in 0..8usize {
        let garbage = vec![0xAAu8; len];
        // panic せず Err を返すことのみを確認する（成功する可能性は無視できるほど
        // 低いが、成功しても不変条件違反ではない）。
        let _ = decode_row(&schema, &garbage);
    }
}

// --- エラーメッセージにテナント ID・値本体を含めない --------------------------

#[test]
fn error_messages_do_not_leak_tenant_id_or_value_body() {
    let schema = single_col_schema(ColumnType::Integer);
    let secret_tenant = "super-secret-tenant-xyz";
    let result = encode_row(&schema, secret_tenant, Visibility::Public, &[Value::Null]);
    let err = result.expect_err("non-nullable column with Null must fail");
    let msg = err.to_string();
    assert!(
        !msg.contains(secret_tenant),
        "error message must not leak tenant id: {msg}"
    );

    let text_schema = single_col_schema(ColumnType::Text);
    let secret_value = "top-secret-value-do-not-leak";
    let oversized = format!("{secret_value}{}", "a".repeat(MAX_TEXT_FIELD_LEN as usize));
    let result = encode_row(
        &text_schema,
        "tenant-a",
        Visibility::Public,
        &[Value::Text(oversized)],
    );
    let err = result.expect_err("oversized text must fail");
    let msg = err.to_string();
    assert!(
        !msg.contains(secret_value),
        "error message must not leak value body: {msg}"
    );
}

// --- 固定シードの自作乱数によるスイープ（依存追加なし） -----------------------

/// splitmix64（依存なしの決定的擬似乱数。proptest 等は使わない方針）。
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
}

#[test]
fn integer_and_bigint_sweep_roundtrips_bit_exact() {
    let mut rng = SplitMix64::new(0x897_1234_5678_9abc);
    for _ in 0..256 {
        let n = rng.next() as i32;
        roundtrip_single(ColumnType::Integer, Value::Integer(n));
        let b = rng.next() as i64;
        roundtrip_single(ColumnType::BigInt, Value::BigInt(b));
    }
}

#[test]
fn uuid_sweep_roundtrips_bit_exact() {
    let mut rng = SplitMix64::new(0x897_dead_beef_0001);
    for _ in 0..128 {
        let mut bytes = [0u8; 16];
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&rng.next().to_le_bytes());
        }
        roundtrip_single(ColumnType::Uuid, Value::Uuid(Uuid::from_bytes(bytes)));
    }
}

#[test]
fn date_and_timestamp_sweep_within_valid_range_roundtrips_bit_exact() {
    let mut rng = SplitMix64::new(0x897_cafe_babe_0002);
    let date_span = (DATE_MAX_DAYS as i64) - (DATE_MIN_DAYS as i64) + 1;
    let ts_span = TIMESTAMP_MAX_MICROS - TIMESTAMP_MIN_MICROS + 1;
    for _ in 0..128 {
        let day = DATE_MIN_DAYS as i64 + (rng.next() as i64).rem_euclid(date_span);
        roundtrip_single(ColumnType::Date, Value::Date(day as i32));
        let micros = TIMESTAMP_MIN_MICROS + (rng.next() as i64).rem_euclid(ts_span);
        roundtrip_single(ColumnType::Timestamp, Value::Timestamp(micros));
    }
}

// ケース表の非 vacuous 性を保つための最終確認: MAX_ENUM_LABELS は本ファイルの
// 語彙数（3）より大きいことを前提にしている（enum_column.rs の既存契約と同じ
// 想定を明示する回帰）。
#[test]
fn enum_labels_fixture_is_well_within_max_enum_labels() {
    assert!(enum_labels().len() < MAX_ENUM_LABELS);
}

// --- Issue #1193: 配列の要素型拡大・NULL 要素 ----------------------------

fn array_schema_of(elem: ArrayElemType) -> ColumnType {
    ColumnType::Array(ArrayType::new(elem, 16).expect("array ty"))
}

#[test]
fn array_new_elem_types_roundtrip_with_and_without_null_elements() {
    let u1 = Uuid::from_bytes([0x11; 16]);
    let u2 = Uuid::from_bytes([0xee; 16]);
    let cases: Vec<(ArrayElemType, ArrayValue)> = vec![
        (
            ArrayElemType::Integer,
            ArrayValue::Integer(vec![Some(i32::MIN), None, Some(0), Some(i32::MAX)]),
        ),
        (
            ArrayElemType::BigInt,
            ArrayValue::BigInt(vec![None, Some(i64::MIN), Some(i64::MAX)]),
        ),
        (
            ArrayElemType::Real,
            ArrayValue::Real(vec![Some(1.5), None, Some(-2.25)]),
        ),
        (
            ArrayElemType::Double,
            ArrayValue::Double(vec![Some(-0.5), Some(1e300), None]),
        ),
        (
            ArrayElemType::Date,
            ArrayValue::Date(vec![Some(DATE_MIN_DAYS), None, Some(DATE_MAX_DAYS)]),
        ),
        (
            ArrayElemType::Timestamp,
            ArrayValue::Timestamp(vec![
                Some(TIMESTAMP_MIN_MICROS),
                None,
                Some(TIMESTAMP_MAX_MICROS),
            ]),
        ),
        (
            ArrayElemType::Uuid,
            ArrayValue::Uuid(vec![Some(u1), None, Some(u2)]),
        ),
        (
            ArrayElemType::Text,
            ArrayValue::Text(vec![
                None,
                Some("NULL".to_string()),
                Some(String::new()),
                None,
            ]),
        ),
        (
            ArrayElemType::Bool,
            ArrayValue::Bool(vec![Some(true), None, Some(false)]),
        ),
    ];
    for (elem, value) in cases {
        // NULL 要素あり・NULL 要素なし・空配列・全要素 NULL のいずれも往復する。
        roundtrip_single(array_schema_of(elem), Value::Array(value.clone()));
        let empty = match &value {
            ArrayValue::Text(_) => ArrayValue::Text(vec![]),
            ArrayValue::Bool(_) => ArrayValue::Bool(vec![]),
            ArrayValue::Integer(_) => ArrayValue::Integer(vec![]),
            ArrayValue::BigInt(_) => ArrayValue::BigInt(vec![]),
            ArrayValue::Real(_) => ArrayValue::Real(vec![]),
            ArrayValue::Double(_) => ArrayValue::Double(vec![]),
            ArrayValue::Date(_) => ArrayValue::Date(vec![]),
            ArrayValue::Timestamp(_) => ArrayValue::Timestamp(vec![]),
            ArrayValue::Uuid(_) => ArrayValue::Uuid(vec![]),
        };
        roundtrip_single(array_schema_of(elem), Value::Array(empty));
    }
    roundtrip_single(
        array_schema_of(ArrayElemType::Integer),
        Value::Array(ArrayValue::Integer(vec![None, None, None])),
    );
    // 列自体の NULL と、全要素 NULL の配列・空配列は互いに区別される。
    let schema = TableSchema::new(
        "docs",
        vec![ColumnDef::new(
            "v",
            array_schema_of(ArrayElemType::Integer),
            true,
        )],
    );
    let enc = |v: Value| encode_row(&schema, "tenant-a", Visibility::Public, &[v]).expect("encode");
    let null_col = enc(Value::Null);
    let all_null = enc(Value::Array(ArrayValue::Integer(vec![None])));
    let empty = enc(Value::Array(ArrayValue::Integer(vec![])));
    assert_ne!(null_col, all_null);
    assert_ne!(all_null, empty);
    assert_ne!(null_col, empty);
}

#[test]
fn array_existing_text_and_bool_bytes_are_unchanged_without_null_elements() {
    // NULL 要素を含まない TEXT／BOOLEAN 配列の行バイト（flags=0x00・要素数 u32 LE・
    // ペイロード長 u32 LE・要素列）は #888 から不変（永続形式の互換性）。
    let schema = single_col_schema(array_schema_of(ArrayElemType::Text));
    let encoded = encode_row(
        &schema,
        "t",
        Visibility::Public,
        &[Value::Array(ArrayValue::Text(vec![
            Some("a".to_string()),
            Some("bc".to_string()),
        ]))],
    )
    .expect("encode");
    let tail: Vec<u8> = vec![
        0x01, // presence
        0x00, // flags
        2,
        0,
        0,
        0, // count
        5 + 6,
        0,
        0,
        0, // payload length
        1,
        0,
        0,
        0,
        b'a',
        2,
        0,
        0,
        0,
        b'b',
        b'c',
    ];
    assert!(
        encoded.ends_with(&tail),
        "TEXT array row bytes changed: {encoded:?}"
    );

    let schema = single_col_schema(array_schema_of(ArrayElemType::Bool));
    let encoded = encode_row(
        &schema,
        "t",
        Visibility::Public,
        &[Value::Array(ArrayValue::Bool(vec![
            Some(true),
            Some(false),
        ]))],
    )
    .expect("encode");
    let tail: Vec<u8> = vec![0x01, 0x00, 2, 0, 0, 0, 2, 0, 0, 0, 1, 0];
    assert!(encoded.ends_with(&tail), "BOOL array row bytes changed");
}

#[test]
fn array_null_bitmap_layout_is_lsb_first_and_canonical() {
    let schema = single_col_schema(array_schema_of(ArrayElemType::Integer));
    let encoded = encode_row(
        &schema,
        "t",
        Visibility::Public,
        &[Value::Array(ArrayValue::Integer(vec![
            Some(7),
            None,
            Some(9),
        ]))],
    )
    .expect("encode");
    // flags=0x01・count=3・payload = bitmap(1 バイト: 0b010) + 非 NULL 要素 2 個(4B×2)。
    let tail: Vec<u8> = vec![
        0x01,
        0x01,
        3,
        0,
        0,
        0,
        9,
        0,
        0,
        0,
        0b0000_0010,
        7,
        0,
        0,
        0,
        9,
        0,
        0,
        0,
    ];
    assert!(encoded.ends_with(&tail), "null bitmap layout: {encoded:?}");

    // 非正準形（flags=0x01 なのにビットが全部 0）は decode で拒否される。
    let mut bad = encoded.clone();
    let bitmap_index = bad.len() - 9;
    bad[bitmap_index] = 0;
    assert!(matches!(
        decode_row(&schema, &bad),
        Err(RowCodecError::Invalid(_))
    ));
    // 余りビット（count=3 の上位 5 ビット）が 1 の場合も拒否される。
    let mut bad = encoded.clone();
    bad[bitmap_index] = 0b1000_0010;
    assert!(matches!(
        decode_row(&schema, &bad),
        Err(RowCodecError::Invalid(_))
    ));
}

#[test]
fn array_encode_rejects_out_of_range_and_non_finite_elements() {
    let schema = single_col_schema(array_schema_of(ArrayElemType::Date));
    assert!(matches!(
        encode_row(
            &schema,
            "t",
            Visibility::Public,
            &[Value::Array(ArrayValue::Date(vec![Some(
                DATE_MAX_DAYS + 1
            )]))],
        ),
        Err(RowCodecError::Invalid(_))
    ));
    let schema = single_col_schema(array_schema_of(ArrayElemType::Real));
    assert!(matches!(
        encode_row(
            &schema,
            "t",
            Visibility::Public,
            &[Value::Array(ArrayValue::Real(vec![Some(f32::NAN)]))],
        ),
        Err(RowCodecError::Invalid(_))
    ));
}
