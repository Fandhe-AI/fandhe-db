//! `engine::row_codec` の統合テスト（TASK-86、対象ビヘイビア: TABLE-7。
//! ポインタ: `docs/spec/04-behavior/data-model.md`）。
//!
//! `row_codec` は `redb::Database` に依存しないため、`tests/catalog.rs` /
//! `tests/persistence.rs` のような一意 DB パスヘルパは不要。ヘルパを複製せず
//! `TableSchema`/`Value` を直接組み立てる小さなテストに留める（既存ファイルの
//! 流儀を踏襲しつつ、本モジュールに合わせて簡素化する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::row_codec::{decode_row, encode_row, RowCodecError, Value};
use engine::storage::Visibility;

fn mixed_schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

#[test]
fn encode_then_decode_roundtrip_with_tenant_and_visibility() {
    let schema = mixed_schema();
    let values = vec![
        Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
        Value::Text("/docs/readme.md".to_string()),
        Value::Text("ja".to_string()),
    ];
    let encoded = encode_row(&schema, "tenant-x", Visibility::Public, &values).expect("encode");
    let decoded = decode_row(&schema, &encoded).expect("decode");
    assert_eq!(decoded.tenant_id, "tenant-x");
    assert_eq!(decoded.visibility, Visibility::Public);
    assert_eq!(decoded.values, values);
}

#[test]
fn short_field_length_overflow_is_rejected_not_truncated() {
    // tenant_id 長フィールド（u8）の上限（255 バイト）を超える入力は Err になり、
    // 剰余に切り詰められた値（256 % 256 == 0 等）で成功してはならない（TABLE-7 の核心）。
    let schema = mixed_schema();
    let values = vec![
        Value::Vector(vec![0.0, 0.0, 0.0, 0.0]),
        Value::Text("/x".to_string()),
        Value::Null,
    ];
    let oversized_tenant = "a".repeat(256);
    let result = encode_row(&schema, &oversized_tenant, Visibility::Public, &values);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn text_field_length_overflow_is_rejected() {
    let schema = TableSchema::new(
        "docs",
        vec![ColumnDef::new("body", ColumnType::Text, false)],
    );
    // MAX_TEXT_FIELD_LEN(4 MiB) を超える body は Err。
    let oversized_body = "a".repeat(4 * 1024 * 1024 + 1);
    let values = vec![Value::Text(oversized_body)];
    let result = encode_row(&schema, "tenant-x", Visibility::Public, &values);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn vector_dimension_mismatch_is_rejected() {
    let schema = mixed_schema();
    let values = vec![
        Value::Vector(vec![0.1, 0.2]), // 宣言次元 4 に対し 2
        Value::Text("/x".to_string()),
        Value::Null,
    ];
    let result = encode_row(&schema, "tenant-x", Visibility::Public, &values);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn unknown_visibility_byte_is_rejected() {
    let schema = TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)]);
    // ヘッダを手組みし、visibility バイトに未知値 (0xEE) を入れる。
    let buf: Vec<u8> = vec![1 /* format version */, 0xEE, 1, b't'];
    let result = decode_row(&schema, &buf);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn unknown_format_version_is_rejected() {
    let schema = mixed_schema();
    let result = decode_row(&schema, &[0xFF]);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn truncated_buffer_at_vector_field_is_rejected() {
    let schema = mixed_schema();
    let values = vec![
        Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
        Value::Text("/x".to_string()),
        Value::Null,
    ];
    let encoded = encode_row(&schema, "tenant-x", Visibility::Public, &values).expect("encode");
    // embedding のペイロード途中で切断する（ヘッダ + presence + dim(4) の直後で切る）。
    let cut = 3 + "tenant-x".len() + 1 + 4 + 4; // header + tenant + presence + dim + 4 bytes of f32
    let truncated = &encoded[..cut];
    let result = decode_row(&schema, truncated);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn missing_trailing_nullable_column_decodes_as_null() {
    // TABLE-5 前提: ADD COLUMN で追加された nullable な末尾列を持たない既存行を
    // デコードすると Null として読める。
    let schema = mixed_schema();
    let values_without_lang = vec![
        Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
        Value::Text("/x".to_string()),
    ];
    // lang 列を含めずに手組みする（encode_row は values 不足分を Null 補完するため、
    // ここでは values を 2 要素のみ渡して同じ状況を再現する）。
    let encoded = encode_row(
        &schema,
        "tenant-x",
        Visibility::Public,
        &values_without_lang,
    )
    .expect("encode");
    let decoded = decode_row(&schema, &encoded).expect("decode");
    assert_eq!(decoded.values.len(), 3);
    assert_eq!(decoded.values[2], Value::Null);
}

#[test]
fn missing_trailing_non_nullable_column_is_rejected() {
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    );
    let values = vec![Value::Text("only-body".to_string())];
    // encode_row 自体が non-nullable 欠落を拒否することを確認する。
    let result = encode_row(&schema, "tenant-x", Visibility::Public, &values);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn null_for_non_nullable_column_is_rejected_on_encode() {
    let schema = mixed_schema();
    let values = vec![
        Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
        Value::Null, // path は non-nullable
        Value::Null,
    ];
    let result = encode_row(&schema, "tenant-x", Visibility::Public, &values);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn declared_length_exceeding_remaining_buffer_is_rejected_before_allocation() {
    let schema = TableSchema::new(
        "docs",
        vec![ColumnDef::new("body", ColumnType::Text, false)],
    );
    let mut buf: Vec<u8> = vec![1 /* format version */, 0x01 /* Public */, 8];
    buf.extend_from_slice(b"tenant-x");
    buf.push(1 /* presence: value */);
    buf.extend_from_slice(&u32::MAX.to_le_bytes()); // 宣言長が残りバッファを大幅超過
    buf.extend_from_slice(b"short");
    let result = decode_row(&schema, &buf);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}

#[test]
fn non_empty_body_roundtrips_without_truncation() {
    let schema = TableSchema::new(
        "docs",
        vec![ColumnDef::new("body", ColumnType::Text, false)],
    );
    let body = "本文が単一保管される行の往復一致を確認する（TASK-86 の複製方針判断）。".repeat(20);
    let values = vec![Value::Text(body.clone())];
    let encoded = encode_row(&schema, "tenant-x", Visibility::Public, &values).expect("encode");
    let decoded = decode_row(&schema, &encoded).expect("decode");
    assert_eq!(decoded.values[0], Value::Text(body));
}

#[test]
fn too_many_values_for_schema_is_rejected() {
    let schema = TableSchema::new(
        "docs",
        vec![ColumnDef::new("body", ColumnType::Text, false)],
    );
    let values = vec![
        Value::Text("a".to_string()),
        Value::Text("b".to_string()), // スキーマの列数(1)を超える
    ];
    let result = encode_row(&schema, "tenant-x", Visibility::Public, &values);
    assert!(matches!(result, Err(RowCodecError::Invalid(_))));
}
