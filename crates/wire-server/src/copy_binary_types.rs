//! COPY binary 形式の「列の型ごとの値変換層」（WIRE-23 (3)・Issue #1768・親 #1766）。
//!
//! 役割: フレーム層 [`crate::copy_binary`]（型を知らない）が切り出したフィールドのバイト列を
//! 列の型に従って正規テキストへ復号し（COPY FROM。#1769 の `run_copy_from` が呼ぶ）、
//! 結果セルを型ごとの binary 値へ符号化する（COPY TO。#1770 の `run_copy_to` が呼ぶ）。
//! 復号結果は engine の `bind_copy_record` が受ける `&[Option<String>]` と同じ形で、
//! Bind のバイナリ引数と同じく「バイナリ → 正規テキスト → engine の型付き束縛」とすることで
//! binary ≡ text を構造的に保証する（第 2 の値検証・第 2 の実行器を作らない）。
//!
//! 対応型の単一情報源は `result_encoder::column_binary_support`（Bind・DataRow と同一集合）で、
//! 本モジュールは第 2 の型表を持たない。非対応列を含む列集合は [`resolve_columns`] が
//! 一括で `0A000` に拒否する。COPY binary はフィールドに OID を持たず列の型が唯一の
//! 表現のため、Bind の宣言 OID 照合（`extended_query::binary_param_kind`）は使わない。
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを使わない。エラー文言は固定文字列で、
//! 受信値・他テナントの情報を含めない。
#![cfg_attr(not(test), allow(dead_code))] // COPY TO 側（`encode_row_fields`）の呼び出し元は #1770 が結線する。

use engine::error_format::ErrorClass;
use engine::sql::exec::{Cell, ColumnMeta, ResultRow};

use crate::binary_param::{self, BinaryParam, BinaryParamError};
use crate::result_encoder::{self, WireType};

/// 列ごとのコーデック（[`resolve_columns`] が作る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CopyBinaryColumn {
    /// 列が公告する型（COPY TO の符号化に使う）。
    wire_type: WireType,
    /// COPY FROM の復号種別。
    decode: BinaryParam,
}

/// 値変換層のエラー。`sqlstate`／`client_message`／`error_class` で wire への写像を返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyBinaryValueError {
    /// 非対応の列型（0A000）。CopyIn／CopyOutResponse の前に判定する。
    Unsupported,
    /// 長さ不正・numeric の外部表現不正・不正 UTF-8・NUL を含む値（22P03）。
    InvalidRepresentation,
    /// TIME・TIMESTAMPTZ・INTERVAL の範囲外（22008）。
    DatetimeOutOfRange { label: &'static str },
    /// numeric の絶対上限超過（22003）。
    NumericOutOfRange,
    /// 内部不整合（件数・型と Cell の不一致。XX000）。
    Encode,
}

impl CopyBinaryValueError {
    /// SQLSTATE。
    pub(crate) fn sqlstate(&self) -> &'static str {
        match self {
            Self::Unsupported => "0A000",
            Self::InvalidRepresentation => "22P03",
            Self::DatetimeOutOfRange { .. } => "22008",
            Self::NumericOutOfRange => "22003",
            Self::Encode => "XX000",
        }
    }

    /// クライアント向けの固定メッセージ（受信値を含めない）。
    pub(crate) fn client_message(&self) -> String {
        match self {
            Self::Unsupported => "COPY binary format is not supported for a column type".into(),
            Self::InvalidRepresentation => "incorrect binary data format".into(),
            Self::DatetimeOutOfRange { label } => format!("{label} out of range"),
            Self::NumericOutOfRange => "numeric field overflow".into(),
            Self::Encode => "internal error while encoding COPY binary data".into(),
        }
    }

    /// 既存の [`ErrorClass`] への写像（新 variant は不要）。
    pub(crate) fn error_class(&self) -> ErrorClass {
        match self {
            Self::Unsupported => ErrorClass::FeatureNotSupported,
            Self::InvalidRepresentation => ErrorClass::InvalidBinaryRepresentation,
            Self::DatetimeOutOfRange { .. } => ErrorClass::DatetimeFieldOverflow,
            Self::NumericOutOfRange => ErrorClass::NumericOutOfRange,
            Self::Encode => ErrorClass::InternalError,
        }
    }
}

/// 列集合のコーデックを解決する。1 列でも非対応なら `0A000`（fail-closed）。
/// **CopyInResponse／CopyOutResponse を送る前** に呼ぶこと（#1769・#1770 向けの契約）。
pub(crate) fn resolve_columns(
    metas: &[ColumnMeta],
) -> Result<Vec<CopyBinaryColumn>, CopyBinaryValueError> {
    let mut cols = Vec::with_capacity(metas.len());
    for meta in metas {
        if !result_encoder::column_binary_support(meta) {
            return Err(CopyBinaryValueError::Unsupported);
        }
        let wire_type = result_encoder::column_wire_type(meta);
        let decode = binary_param::binary_param_for_wire_type(wire_type);
        if decode == BinaryParam::Unsupported {
            return Err(CopyBinaryValueError::Unsupported);
        }
        cols.push(CopyBinaryColumn { wire_type, decode });
    }
    Ok(cols)
}

/// 1 フィールドを正規テキストへ復号する。
///
/// 長さ不正は WIRE-23 (3) のポインタに従い `22P03`（Bind の `08P01` とは意図的に異なる）。
/// text 系の不正 UTF-8・NUL は PostgreSQL の `22021` ではなく、新 SQLSTATE を入れず
/// `22P03` で拒否側に倒す（既知の相違）。
pub(crate) fn decode_field(
    col: &CopyBinaryColumn,
    raw: &[u8],
) -> Result<String, CopyBinaryValueError> {
    let bytes = binary_param::decode_to_text(col.decode, raw).map_err(|e| match e {
        BinaryParamError::InvalidLength | BinaryParamError::InvalidRepresentation => {
            CopyBinaryValueError::InvalidRepresentation
        }
        BinaryParamError::OutOfRange => CopyBinaryValueError::DatetimeOutOfRange {
            label: col.decode.out_of_range_label(),
        },
        BinaryParamError::NumericOutOfRange => CopyBinaryValueError::NumericOutOfRange,
    })?;
    let text = String::from_utf8(bytes).map_err(|_| CopyBinaryValueError::InvalidRepresentation)?;
    if col.decode == BinaryParam::Utf8Identity && text.contains('\0') {
        return Err(CopyBinaryValueError::InvalidRepresentation);
    }
    Ok(text)
}

/// 1 タプルを復号する（`BinaryCopyReader::feed` の `on_tuple` の引数をそのまま受ける）。
/// `None`（長さ -1）は透過し、NOT NULL 判定は engine の束縛段が担う。
/// 件数不一致は内部不整合（フレーム層が検証済みの前提）として `Encode`。
pub(crate) fn decode_tuple(
    cols: &[CopyBinaryColumn],
    fields: Vec<Option<Vec<u8>>>,
) -> Result<Vec<Option<String>>, CopyBinaryValueError> {
    if cols.len() != fields.len() {
        return Err(CopyBinaryValueError::Encode);
    }
    cols.iter()
        .zip(fields)
        .map(|(col, f)| f.map(|raw| decode_field(col, &raw)).transpose())
        .collect()
}

/// 1 行を列ごとの binary 値へ符号化する（`None` は NULL）。呼び出し側は `as_deref` で
/// `&[Option<&[u8]>]` にして `copy_binary::encode_tuple_into` へ渡す。
pub(crate) fn encode_row_fields(
    cols: &[CopyBinaryColumn],
    row: &ResultRow,
) -> Result<Vec<Option<Vec<u8>>>, CopyBinaryValueError> {
    if cols.len() != row.cells.len() {
        return Err(CopyBinaryValueError::Encode);
    }
    cols.iter()
        .zip(&row.cells)
        .map(|(col, cell)| match cell {
            Cell::Null => Ok(None),
            other => {
                let mut buf = Vec::new();
                result_encoder::encode_binary_payload_into(Some(col.wire_type), other, &mut buf)
                    .map_err(|_| CopyBinaryValueError::Encode)?;
                Ok(Some(buf))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! WIRE-23 (3) の型別往復・負例（結合テストは #1769 以降）。
    use super::*;
    use crate::copy_binary::{
        encode_header_into, encode_trailer_into, encode_tuple_into, BinaryCopyReader,
        BinaryFrameLimits, CopyBinaryError,
    };
    use engine::catalog::ColumnType;
    use engine::datetime::Interval;
    use engine::numeric::Decimal;

    fn scalar(ty: ColumnType) -> ColumnMeta {
        ColumnMeta::Scalar {
            name: "c".into(),
            ty,
        }
    }

    /// 1 列の値を encode → フレーム化 → 3 バイト刻みで feed → decode して正規テキストを返す。
    fn roundtrip(meta: ColumnMeta, cell: Cell) -> Option<String> {
        let cols = resolve_columns(std::slice::from_ref(&meta)).unwrap();
        let row = ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![cell],
        };
        let fields = encode_row_fields(&cols, &row).unwrap();
        let refs: Vec<Option<&[u8]>> = fields.iter().map(|f| f.as_deref()).collect();
        let mut stream = Vec::new();
        encode_header_into(&mut stream);
        encode_tuple_into(&refs, &mut stream).unwrap();
        encode_trailer_into(&mut stream);
        let mut reader = BinaryCopyReader::new(
            1,
            BinaryFrameLimits {
                max_field_bytes: 1 << 16,
                max_tuple_bytes: 1 << 16,
            },
        )
        .unwrap();
        let mut got = Vec::new();
        for chunk in stream.chunks(3) {
            reader
                .feed::<CopyBinaryError>(chunk, |t| {
                    got.push(t);
                    Ok(())
                })
                .unwrap();
        }
        reader.finish().unwrap();
        assert_eq!(got.len(), 1);
        let mut out = decode_tuple(&cols, got.pop().unwrap()).unwrap();
        out.pop().unwrap()
    }

    fn rt(ty: ColumnType, cell: Cell) -> String {
        roundtrip(scalar(ty), cell).expect("non-null")
    }

    #[test]
    fn integers_round_trip() {
        for v in [i64::from(i16::MIN), -1, 0, 1, i64::from(i16::MAX)] {
            assert_eq!(
                rt(ColumnType::SmallInt, Cell::SignedInteger(v)).parse::<i64>(),
                Ok(v)
            );
        }
        for v in [i64::from(i32::MIN), -1, 0, i64::from(i32::MAX)] {
            assert_eq!(
                rt(ColumnType::Integer, Cell::SignedInteger(v)).parse::<i64>(),
                Ok(v)
            );
        }
        for v in [i64::MIN, -1, 0, i64::MAX] {
            assert_eq!(
                rt(ColumnType::BigInt, Cell::SignedInteger(v)).parse::<i64>(),
                Ok(v)
            );
        }
    }

    #[test]
    fn floats_round_trip() {
        for v in [
            0.0f64,
            -0.0,
            1.5,
            -2.25,
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324,
        ] {
            let t = rt(ColumnType::Double, Cell::Float(v));
            let back = engine::scalar_float::parse_double(&t).unwrap();
            // -0 は engine の text 束縛が +0 に正規化するため値比較（0.0 == -0.0）で確かめる。
            assert_eq!(back, v, "{t}");
        }
        // 非有限値は engine の text 束縛と同じ正規テキストへ復号される（binary ≡ text。
        // 受理可否は engine の束縛段が判定する）。
        assert_eq!(
            rt(ColumnType::Double, Cell::Float(f64::INFINITY)),
            "Infinity"
        );
        assert_eq!(
            rt(ColumnType::Double, Cell::Float(f64::NEG_INFINITY)),
            "-Infinity"
        );
        assert_eq!(rt(ColumnType::Double, Cell::Float(f64::NAN)), "NaN");
        for v in [0.5f32, -0.0, f32::MAX, f32::from_bits(1)] {
            let t = rt(ColumnType::Real, Cell::Float(f64::from(v)));
            let back = engine::scalar_float::parse_real_text(&t).unwrap();
            // -0 は engine の text 束縛が +0 に正規化するため値比較（0.0 == -0.0）で確かめる。
            assert_eq!(back, v, "{t}");
        }
        assert_eq!(rt(ColumnType::Real, Cell::Float(f64::INFINITY)), "Infinity");
        assert_eq!(rt(ColumnType::Real, Cell::Float(f64::NAN)), "NaN");
    }

    #[test]
    fn bool_bytea_uuid_round_trip() {
        assert_eq!(rt(ColumnType::Boolean, Cell::Bool(true)), "t");
        assert_eq!(rt(ColumnType::Boolean, Cell::Bool(false)), "f");
        let cols = resolve_columns(&[scalar(ColumnType::Boolean)]).unwrap();
        for (b, want) in [(0u8, "f"), (1, "t"), (2, "t")] {
            assert_eq!(decode_field(&cols[0], &[b]).unwrap(), want);
        }
        for data in [vec![], vec![0u8, 0xff, 0x7f], (0..=255u8).collect()] {
            let t = rt(ColumnType::Bytea, Cell::Bytes(data.clone()));
            assert_eq!(engine::bytea::parse_hex_text(&t).unwrap(), data);
        }
        let u = engine::uuid::Uuid::from_bytes([7; 16]);
        let t = rt(ColumnType::Uuid, Cell::Uuid(u));
        assert_eq!(engine::uuid::parse_uuid_text(&t).unwrap(), u);
    }

    #[test]
    fn text_family_and_id_round_trip() {
        for s in ["", "abc", "日本語🙂"] {
            assert_eq!(rt(ColumnType::Text, Cell::Text(s.into())), s);
            assert_eq!(rt(ColumnType::Varchar(Some(20)), Cell::Text(s.into())), s);
        }
        assert_eq!(
            rt(ColumnType::Bpchar(5), Cell::Text("ab   ".into())),
            "ab   "
        );
        let t = roundtrip(ColumnMeta::Id, Cell::Integer(42)).unwrap();
        assert_eq!(t.parse::<u64>(), Ok(42));
        let t = roundtrip(ColumnMeta::Id, Cell::Integer(u64::MAX)).unwrap();
        assert_eq!(t.parse::<u64>(), Ok(u64::MAX));
        let t = roundtrip(ColumnMeta::AliasedId { name: "x".into() }, Cell::Integer(7)).unwrap();
        assert_eq!(t.parse::<u64>(), Ok(7));
    }

    #[test]
    fn datetime_round_trip() {
        for m in [0, 1, 43_200_000_000, engine::datetime::TIME_MAX_MICROS] {
            let t = rt(ColumnType::Time, Cell::Time(m));
            assert_eq!(engine::datetime::parse_time(&t).unwrap(), m);
        }
        let tz = engine::datetime::SessionTimeZone::utc();
        for m in [
            engine::datetime::TIMESTAMPTZ_MIN_MICROS,
            -1,
            0,
            1_700_000_000_123_456,
            engine::datetime::TIMESTAMPTZ_MAX_MICROS,
        ] {
            let t = rt(ColumnType::TimestampTz, Cell::TimestampTz(m));
            assert_eq!(
                engine::datetime::parse_timestamptz(&t, &tz).unwrap(),
                m,
                "{t}"
            );
        }
        for iv in [
            Interval {
                months: 0,
                days: 0,
                micros: 0,
            },
            Interval {
                months: -14,
                days: 3,
                micros: -5_000_001,
            },
            Interval {
                months: 12,
                days: -7,
                micros: 86_400_000_000,
            },
        ] {
            let t = rt(ColumnType::Interval, Cell::Interval(iv));
            assert_eq!(engine::datetime::parse_interval(&t).unwrap(), iv, "{t}");
        }
    }

    #[test]
    fn numeric_round_trip() {
        let ty = ColumnType::Numeric {
            precision: 38,
            scale: 10,
        };
        for (u, s) in [
            (0i128, 0u8),
            (-12345, 2),
            (1, 10),
            (123_456_789_012_345_678_901_234_567_890, 5),
            (-1, 0),
        ] {
            let d = Decimal::from_parts(u, s).unwrap();
            let t = rt(ty.clone(), Cell::Numeric(d));
            let back = engine::numeric::parse_literal_exact(&t).unwrap();
            assert_eq!(back.to_string(), d.to_string(), "{t}");
        }
    }

    #[test]
    fn null_passes_through() {
        assert_eq!(roundtrip(scalar(ColumnType::Integer), Cell::Null), None);
    }

    #[test]
    fn fixed_length_errors_are_22p03_with_fixed_message() {
        let cols = resolve_columns(&[scalar(ColumnType::Integer)]).unwrap();
        for raw in [&[0u8; 3][..], &[0u8; 5][..], &[][..]] {
            let e = decode_field(&cols[0], raw).unwrap_err();
            assert_eq!(e.sqlstate(), "22P03");
            assert_eq!(e.client_message(), "incorrect binary data format");
            assert_eq!(e.error_class(), ErrorClass::InvalidBinaryRepresentation);
        }
    }

    #[test]
    fn datetime_out_of_range_is_22008() {
        let t = resolve_columns(&[scalar(ColumnType::Time)]).unwrap();
        let e = decode_field(&t[0], &i64::MAX.to_be_bytes()).unwrap_err();
        assert_eq!(e.sqlstate(), "22008");
        assert_eq!(e.error_class(), ErrorClass::DatetimeFieldOverflow);
        let z = resolve_columns(&[scalar(ColumnType::TimestampTz)]).unwrap();
        for v in [i64::MAX, i64::MIN] {
            let e = decode_field(&z[0], &v.to_be_bytes()).unwrap_err();
            assert_eq!(e.sqlstate(), "22008");
            assert_eq!(e.client_message(), "timestamp out of range");
        }
        let i = resolve_columns(&[scalar(ColumnType::Interval)]).unwrap();
        let mut raw = Vec::new();
        raw.extend_from_slice(&i64::MAX.to_be_bytes());
        raw.extend_from_slice(&i32::MAX.to_be_bytes());
        raw.extend_from_slice(&i32::MAX.to_be_bytes());
        assert_eq!(decode_field(&i[0], &raw).unwrap_err().sqlstate(), "22008");
    }

    #[test]
    fn numeric_errors() {
        let cols = resolve_columns(&[scalar(ColumnType::Numeric {
            precision: 10,
            scale: 2,
        })])
        .unwrap();
        // ndigits=0, weight=0, 不正な符号語, dscale=0
        let bad_sign = [0, 0, 0, 0, 0x12, 0x34, 0, 0];
        assert_eq!(
            decode_field(&cols[0], &bad_sign).unwrap_err().sqlstate(),
            "22P03"
        );
        // 整数部が engine の絶対上限（38 桁）を超える。
        let mut raw = vec![0, 10, 0, 20, 0, 0, 0, 0];
        for _ in 0..10 {
            raw.extend_from_slice(&1u16.to_be_bytes());
        }
        assert_eq!(
            decode_field(&cols[0], &raw).unwrap_err().sqlstate(),
            "22003"
        );
    }

    #[test]
    fn text_rejects_bad_utf8_and_nul() {
        let cols = resolve_columns(&[scalar(ColumnType::Text)]).unwrap();
        for raw in [&[0xffu8, 0xfe][..], b"a\0b"] {
            assert_eq!(decode_field(&cols[0], raw).unwrap_err().sqlstate(), "22P03");
        }
    }

    #[test]
    fn unsupported_columns_are_0a000() {
        let unsupported = [
            scalar(ColumnType::Date),
            scalar(ColumnType::Timestamp),
            scalar(ColumnType::Json),
            scalar(ColumnType::Jsonb),
            scalar(ColumnType::Vector(3)),
            ColumnMeta::Computed {
                name: "x".into(),
                ty: None,
            },
        ];
        for m in &unsupported {
            let e = resolve_columns(&[scalar(ColumnType::Integer), m.clone()]).unwrap_err();
            assert_eq!(e, CopyBinaryValueError::Unsupported);
            assert_eq!(e.sqlstate(), "0A000");
            assert_eq!(e.error_class(), ErrorClass::FeatureNotSupported);
        }
        assert!(resolve_columns(&[ColumnMeta::Id, scalar(ColumnType::Text)]).is_ok());
    }

    /// 受理 ⇔ `column_binary_support`（単一情報源）。
    #[test]
    fn resolve_matches_column_binary_support() {
        let metas = [
            ColumnMeta::Id,
            ColumnMeta::AliasedId { name: "a".into() },
            scalar(ColumnType::Text),
            scalar(ColumnType::Varchar(None)),
            scalar(ColumnType::Bpchar(3)),
            scalar(ColumnType::SmallInt),
            scalar(ColumnType::Integer),
            scalar(ColumnType::BigInt),
            scalar(ColumnType::Real),
            scalar(ColumnType::Double),
            scalar(ColumnType::Boolean),
            scalar(ColumnType::Date),
            scalar(ColumnType::Timestamp),
            scalar(ColumnType::Time),
            scalar(ColumnType::TimestampTz),
            scalar(ColumnType::Interval),
            scalar(ColumnType::Bytea),
            scalar(ColumnType::Uuid),
            scalar(ColumnType::Json),
            scalar(ColumnType::Jsonb),
            scalar(ColumnType::Vector(2)),
            scalar(ColumnType::Numeric {
                precision: 5,
                scale: 1,
            }),
            ColumnMeta::Computed {
                name: "c".into(),
                ty: Some(ColumnType::Integer),
            },
            ColumnMeta::Computed {
                name: "c".into(),
                ty: None,
            },
        ];
        for m in &metas {
            assert_eq!(
                resolve_columns(std::slice::from_ref(m)).is_ok(),
                result_encoder::column_binary_support(m),
                "{m:?}"
            );
        }
    }

    #[test]
    fn count_and_type_mismatches_are_xx000() {
        let cols =
            resolve_columns(&[scalar(ColumnType::Integer), scalar(ColumnType::Text)]).unwrap();
        assert_eq!(
            decode_tuple(&cols, vec![None]).unwrap_err(),
            CopyBinaryValueError::Encode
        );
        let row = ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::Null],
        };
        assert_eq!(
            encode_row_fields(&cols, &row).unwrap_err().sqlstate(),
            "XX000"
        );
        let row = ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::Text("x".into()), Cell::Text("y".into())],
        };
        assert_eq!(
            encode_row_fields(&cols, &row).unwrap_err(),
            CopyBinaryValueError::Encode
        );
    }

    /// 固定シードの擬似乱数バイト列で panic しないこと。
    #[test]
    fn random_bytes_never_panic() {
        let metas = [
            scalar(ColumnType::SmallInt),
            scalar(ColumnType::Integer),
            scalar(ColumnType::BigInt),
            scalar(ColumnType::Real),
            scalar(ColumnType::Double),
            scalar(ColumnType::Boolean),
            scalar(ColumnType::Text),
            scalar(ColumnType::Bytea),
            scalar(ColumnType::Uuid),
            scalar(ColumnType::Time),
            scalar(ColumnType::TimestampTz),
            scalar(ColumnType::Interval),
            scalar(ColumnType::Numeric {
                precision: 10,
                scale: 2,
            }),
        ];
        let cols = resolve_columns(&metas).unwrap();
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..2000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = usize::try_from(x % 40).unwrap();
            let raw: Vec<u8> = (0..len)
                .map(|i| ((x >> (i % 8 * 8)) as u8) ^ (i as u8))
                .collect();
            for c in &cols {
                let _ = decode_field(c, &raw);
            }
        }
    }

    /// 符号化側は固定長型で長さ無しの値そのものを返す。
    #[test]
    fn payload_has_no_length_prefix() {
        let cols = resolve_columns(&[scalar(ColumnType::BigInt)]).unwrap();
        let row = ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::SignedInteger(-2)],
        };
        let f = encode_row_fields(&cols, &row).unwrap();
        assert_eq!(f, vec![Some((-2i64).to_be_bytes().to_vec())]);
    }
}
