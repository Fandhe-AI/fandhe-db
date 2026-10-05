//! バイナリ形式の結果エンコーディング（WIRE-14・TASK-218・Issue #936）の
//! 公開 API を通した層 A 結合テスト。
//!
//! **本ファイルの範囲（Phase A）**: `crate::result_encoder` の公開関数
//! （[`wire_server::result_encoder`]）だけを対象とする。拡張クエリ
//! プロトコルの Bind／Describe（#933・#934）が未実装のため、実際に wire
//! 経由でバイナリ形式を要求する経路（Bind の結果形式コード → 本 API への
//! 結線、`0A000` 後の同期回復による接続維持）は本 Issue のスコープ外であり、
//! #934 で wire 経由の結合テストへ拡張される想定（`result_encoder.rs`
//! モジュール冒頭のドキュメンテーションコメント参照）。

use engine::catalog::ColumnType;
use engine::sql::exec::{Cell, ColumnMeta, ResultRow};
use wire_server::result_encoder::{
    encode_data_row, encode_data_row_into_with_formats, encode_row_description,
    encode_row_description_with_formats, validate_binary_formats, BinaryFormatError, FormatCode,
    ResultFormats,
};

fn i32_at(msg: &[u8], idx: usize) -> i32 {
    let bytes: [u8; 4] = msg
        .get(idx..idx + 4)
        .expect("message too short")
        .try_into()
        .expect("slice is exactly 4 bytes");
    i32::from_be_bytes(bytes)
}

fn i16_at(msg: &[u8], idx: usize) -> i16 {
    let bytes: [u8; 2] = msg
        .get(idx..idx + 2)
        .expect("message too short")
        .try_into()
        .expect("slice is exactly 2 bytes");
    i16::from_be_bytes(bytes)
}

fn slice_at(msg: &[u8], idx: usize, len: usize) -> &[u8] {
    msg.get(idx..idx + len).expect("message too short")
}

/// `RowDescription` の各列 format code フィールドのオフセットを、`T` メッセージ
/// のレイアウト（name(NUL) + oid(4) + attnum(2) + type_oid(4) + typlen(2) +
/// typmod(4) + format(2)）から計算するテスト専用ヘルパー。
fn row_description_format_code_at(msg: &[u8], column_name_len: usize, column_offset: usize) -> i16 {
    // body は 'T' + length(4) の直後（idx 5）から。
    let entry_start = 5 + 2 /* field_count */ + column_offset;
    let format_offset = entry_start + column_name_len + 1 /* NUL */ + 4 + 2 + 4 + 2 + 4;
    i16_at(msg, format_offset)
}

#[test]
fn text_column_binary_request_sets_format_code_and_raw_bytes() {
    let columns = vec![ColumnMeta::Scalar {
        name: "lang".to_string(),
        ty: ColumnType::Text,
    }];
    let formats = ResultFormats::new(&[1])
        .resolve(columns.len())
        .expect("resolve");
    validate_binary_formats(&columns, &formats).expect("validate");

    let row_desc =
        encode_row_description_with_formats(&columns, &formats).expect("row description");
    assert_eq!(
        row_description_format_code_at(&row_desc, "lang".len(), 0),
        1,
        "format code must be 1 (binary)"
    );

    let row = ResultRow {
        id: 1,
        score: 0.0,
        cells: vec![Cell::Text("ja".to_string())],
    };
    let mut data_row = Vec::new();
    encode_data_row_into_with_formats(&columns, &row, &formats, &mut data_row).expect("data row");
    // 'D' + length(4) + field_count(2) = idx 7 から cell length(4)。
    let cell_len = i32_at(&data_row, 7) as usize;
    assert_eq!(cell_len, 2);
    assert_eq!(slice_at(&data_row, 11, cell_len), b"ja");
}

#[test]
fn mixed_single_code_applies_binary_to_all_supported_columns() {
    let columns = vec![
        ColumnMeta::Scalar {
            name: "a".to_string(),
            ty: ColumnType::Text,
        },
        ColumnMeta::Scalar {
            name: "b".to_string(),
            ty: ColumnType::Text,
        },
    ];
    // 1 個指定 → 全列へ適用。
    let formats = ResultFormats::new(&[1])
        .resolve(columns.len())
        .expect("resolve");
    assert_eq!(formats, vec![FormatCode::Binary, FormatCode::Binary]);
    validate_binary_formats(&columns, &formats).expect("validate");
}

#[test]
fn mixed_per_column_codes_apply_independently() {
    let columns = vec![
        ColumnMeta::Scalar {
            name: "a".to_string(),
            ty: ColumnType::Text,
        },
        ColumnMeta::Scalar {
            name: "b".to_string(),
            ty: ColumnType::Text,
        },
    ];
    let formats = ResultFormats::new(&[1, 0])
        .resolve(columns.len())
        .expect("resolve");
    assert_eq!(formats, vec![FormatCode::Binary, FormatCode::Text]);
    validate_binary_formats(&columns, &formats).expect("validate");

    let row_desc =
        encode_row_description_with_formats(&columns, &formats).expect("row description");
    assert_eq!(row_description_format_code_at(&row_desc, "a".len(), 0), 1);

    // 2 列目のオフセットは 1 列目の全長ぶん進む。
    let first_entry_len = "a".len() + 1 + 4 + 2 + 4 + 2 + 4 + 2;
    assert_eq!(
        row_description_format_code_at(&row_desc, "b".len(), first_entry_len),
        0
    );
}

#[test]
fn id_column_binary_request_is_rejected_as_feature_not_supported() {
    let columns = vec![ColumnMeta::Id];
    let formats = ResultFormats::new(&[1])
        .resolve(columns.len())
        .expect("resolve");
    let err = validate_binary_formats(&columns, &formats).unwrap_err();
    assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
    assert_eq!(
        err.error_class(),
        engine::error_format::ErrorClass::FeatureNotSupported
    );
    assert_eq!(err.error_class().wire_code(), "0A000");
}

#[test]
fn vector_column_binary_request_is_rejected_as_feature_not_supported() {
    let columns = vec![ColumnMeta::Scalar {
        name: "embedding".to_string(),
        ty: ColumnType::Vector(4),
    }];
    let formats = ResultFormats::new(&[1])
        .resolve(columns.len())
        .expect("resolve");
    let err = validate_binary_formats(&columns, &formats).unwrap_err();
    assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
    assert_eq!(err.error_class().wire_code(), "0A000");
}

#[test]
fn enum_column_binary_request_is_rejected_as_feature_not_supported() {
    // `EnumTypeDef` はフィールドが private で `Storage::create_enum_type` 経由
    // でしか構築できないため（Issue #890）、テスト専用の使い捨て DB を開いて
    // 型登録だけ行う（`ColumnMeta::Scalar { ty }` の判定は型定義の中身
    // 〔語彙〕を一切参照しないため、これで十分）。
    let path = std::env::temp_dir().join(format!(
        "wire14-binary-format-enum-{}-{}.redb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    let storage = engine::storage::Storage::open(&path).expect("open throwaway storage");
    let def = storage
        .create_enum_type("mood", vec!["happy".to_string()])
        .expect("create enum type");
    let columns = vec![ColumnMeta::Scalar {
        name: "mood".to_string(),
        ty: ColumnType::Enum(def),
    }];
    let formats = ResultFormats::new(&[1])
        .resolve(columns.len())
        .expect("resolve");
    let err = validate_binary_formats(&columns, &formats).unwrap_err();
    assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
    assert_eq!(err.error_class().wire_code(), "0A000");
    drop(storage);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn computed_column_binary_request_is_rejected_as_feature_not_supported() {
    let columns = vec![ColumnMeta::Computed {
        name: "expr".to_string(),
        ty: None,
    }];
    let formats = ResultFormats::new(&[1])
        .resolve(columns.len())
        .expect("resolve");
    let err = validate_binary_formats(&columns, &formats).unwrap_err();
    assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
    assert_eq!(err.error_class().wire_code(), "0A000");
}

/// Issue #1407: `Computed{ty: Some(対応型)}` は通り、非対応型は `0A000`。
#[test]
fn computed_column_binary_request_follows_static_type() {
    for ty in [
        ColumnType::BigInt,
        ColumnType::Integer,
        ColumnType::Real,
        ColumnType::Double,
        ColumnType::Boolean,
        ColumnType::Text,
    ] {
        let columns = vec![ColumnMeta::Computed {
            name: "agg".to_string(),
            ty: Some(ty.clone()),
        }];
        let formats = ResultFormats::new(&[1])
            .resolve(columns.len())
            .expect("resolve");
        assert!(
            validate_binary_formats(&columns, &formats).is_ok(),
            "{ty:?}"
        );
    }
    for ty in [
        ColumnType::Numeric {
            precision: 20,
            scale: 0,
        },
        ColumnType::Date,
        ColumnType::Timestamp,
    ] {
        let columns = vec![ColumnMeta::Computed {
            name: "agg".to_string(),
            ty: Some(ty.clone()),
        }];
        let formats = ResultFormats::new(&[1])
            .resolve(columns.len())
            .expect("resolve");
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(
            err,
            BinaryFormatError::UnsupportedType { column_index: 0 },
            "{ty:?}"
        );
        assert_eq!(err.error_class().wire_code(), "0A000");
    }
}

#[test]
fn json_column_binary_request_is_rejected_as_feature_not_supported() {
    // `JSON`／`JSONB` 列（TABLE-14・Issue #889）は BYTEA と同じ理由（値の実体が
    // `Cell::Json` の格納テキストで `Text` の単純な UTF-8 生バイト表現とは
    // 意味論が異なる）で fail-closed に非対応とする。
    for ty in [ColumnType::Json, ColumnType::Jsonb] {
        let columns = vec![ColumnMeta::Scalar {
            name: "doc".to_string(),
            ty: ty.clone(),
        }];
        let formats = ResultFormats::new(&[1])
            .resolve(columns.len())
            .expect("resolve");
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(
            err,
            BinaryFormatError::UnsupportedType { column_index: 0 },
            "ty={ty:?}"
        );
        assert_eq!(err.error_class().wire_code(), "0A000", "ty={ty:?}");
    }
}

#[test]
fn format_count_mismatch_is_protocol_violation() {
    let err = ResultFormats::new(&[0, 1]).resolve(3).unwrap_err();
    assert_eq!(err, BinaryFormatError::FormatCountMismatch);
    assert_eq!(err.error_class().wire_code(), "08P01");
}

#[test]
fn invalid_format_code_value_is_protocol_violation() {
    let err = ResultFormats::new(&[7]).resolve(1).unwrap_err();
    assert_eq!(err, BinaryFormatError::InvalidFormatCode);
    assert_eq!(err.error_class().wire_code(), "08P01");
}

/// 受け入れ条件 4: バイナリ形式を一切要求しない（全列テキスト）場合の出力は
/// 既存エンコーダと完全に同一。
#[test]
fn no_binary_request_preserves_existing_text_output() {
    let columns = vec![
        ColumnMeta::Id,
        ColumnMeta::Scalar {
            name: "lang".to_string(),
            ty: ColumnType::Text,
        },
        ColumnMeta::Scalar {
            name: "embedding".to_string(),
            ty: ColumnType::Vector(3),
        },
        ColumnMeta::Computed {
            name: "expr".to_string(),
            ty: None,
        },
    ];
    let legacy_row_desc = encode_row_description(&columns).expect("legacy row description");
    let formats = ResultFormats::new(&[])
        .resolve(columns.len())
        .expect("resolve");
    let via_formats =
        encode_row_description_with_formats(&columns, &formats).expect("row description");
    assert_eq!(legacy_row_desc, via_formats);

    let row = ResultRow {
        id: 42,
        score: 0.0,
        cells: vec![
            Cell::Integer(42),
            Cell::Text("ja".to_string()),
            Cell::Vector(vec![1.0, 2.0, 3.0]),
            Cell::Bool(true),
        ],
    };
    let legacy_data_row = encode_data_row(&columns, &row).expect("legacy data row");
    let mut via_formats_row = Vec::new();
    encode_data_row_into_with_formats(&columns, &row, &formats, &mut via_formats_row)
        .expect("data row");
    assert_eq!(legacy_data_row, via_formats_row);
}
