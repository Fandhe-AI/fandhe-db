//! Bind のバイナリ形式パラメータ（format code = 1）を正規テキストへ復号する
//! （WIRE-14・TASK-218・Issue #1345）。
//!
//! 役割: `extended_query` の Bind 処理（wire-server の接続ハンドラから呼ばれる）が
//! スロットごとの [`BinaryParam`] に従って受信バイト列を PostgreSQL の受信形式
//! （ネットワークバイトオーダー）で解釈し、テキスト形式で送った場合と同一の
//! 文字列へ変換する。変換後の値は engine の `bind_prepared`（UTF-8・NUL 検証・
//! 型付きリテラル置換）へそのまま渡るため、バイナリ ≡ テキストが構造的に
//! 保証される（第 2 の実行器・第 2 の値検証を作らない）。結果側の符号化は
//! [`crate::result_encoder::binary`] が担い、本モジュールはその逆変換にあたる。
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを使わず、固定長は
//! `try_from` による完全一致で検証する（不一致は [`BinaryParamError`]・`08P01`）。

/// スロットのバイナリ復号種別（Parse 時に確定。`BinaryParam::Unsupported` は
/// バイナリ指定を `0A000` で拒否する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryParam {
    /// バイナリ非対応（fail-closed）。
    Unsupported,
    /// text 系スロット。受信バイト列を UTF-8 の恒等表現としてそのまま渡す。
    Utf8Identity,
    /// int4（4 バイト）。
    Int4,
    /// int8（8 バイト）。
    Int8,
    /// float4（4 バイト）。
    Float4,
    /// float8（8 バイト）。
    Float8,
    /// bool（1 バイト）。
    Bool,
    /// bytea（任意長。`\x` 16 進テキストへ変換）。
    Bytea,
    /// uuid（16 バイト）。
    Uuid,
}

/// バイナリ値の長さが型の受信形式と合わない（`08P01`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BinaryParamError;

/// float を PostgreSQL のテキスト入力が受理する表記へ変換する（有限値は最短往復桁）。
fn float_text<T: std::fmt::Display>(v: &T, nan: bool, inf: bool, neg: bool) -> String {
    if nan {
        "NaN".to_string()
    } else if inf {
        if neg {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        }
    } else {
        v.to_string()
    }
}

/// `raw` を `kind` の受信形式として復号し、テキスト形式と同じ表現のバイト列を返す。
/// `Unsupported` は呼び出し側が事前に弾く契約だが、念のため拒否する（fail-closed）。
pub(crate) fn decode_to_text(kind: BinaryParam, raw: &[u8]) -> Result<Vec<u8>, BinaryParamError> {
    let text = match kind {
        BinaryParam::Unsupported => return Err(BinaryParamError),
        BinaryParam::Utf8Identity => return Ok(raw.to_vec()),
        BinaryParam::Int4 => {
            let b = <[u8; 4]>::try_from(raw).map_err(|_| BinaryParamError)?;
            i32::from_be_bytes(b).to_string()
        }
        BinaryParam::Int8 => {
            let b = <[u8; 8]>::try_from(raw).map_err(|_| BinaryParamError)?;
            i64::from_be_bytes(b).to_string()
        }
        BinaryParam::Float4 => {
            let b = <[u8; 4]>::try_from(raw).map_err(|_| BinaryParamError)?;
            let v = f32::from_bits(u32::from_be_bytes(b));
            float_text(&v, v.is_nan(), v.is_infinite(), v.is_sign_negative())
        }
        BinaryParam::Float8 => {
            let b = <[u8; 8]>::try_from(raw).map_err(|_| BinaryParamError)?;
            let v = f64::from_bits(u64::from_be_bytes(b));
            float_text(&v, v.is_nan(), v.is_infinite(), v.is_sign_negative())
        }
        BinaryParam::Bool => {
            // PostgreSQL の `boolrecv` と同じく 0 以外は真。
            let [b] = <[u8; 1]>::try_from(raw).map_err(|_| BinaryParamError)?;
            if b != 0 { "t" } else { "f" }.to_string()
        }
        BinaryParam::Uuid => {
            let b = <[u8; 16]>::try_from(raw).map_err(|_| BinaryParamError)?;
            engine::uuid::Uuid::from_bytes(b).to_string()
        }
        BinaryParam::Bytea => engine::bytea::format_hex_text(raw),
    };
    Ok(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result_encoder::binary;

    fn dec(kind: BinaryParam, raw: &[u8]) -> String {
        String::from_utf8(decode_to_text(kind, raw).expect("decode")).expect("utf8")
    }

    #[test]
    fn ints_round_trip_including_extremes() {
        for v in [0, 1, -1, i32::MIN, i32::MAX] {
            assert_eq!(dec(BinaryParam::Int4, &binary::int4(v)), v.to_string());
        }
        for v in [0, 1, -1, i64::MIN, i64::MAX] {
            assert_eq!(dec(BinaryParam::Int8, &binary::int8(v)), v.to_string());
        }
    }

    #[test]
    fn floats_cover_special_values() {
        assert_eq!(dec(BinaryParam::Float8, &binary::float8(1.5)), "1.5");
        assert_eq!(dec(BinaryParam::Float8, &binary::float8(f64::NAN)), "NaN");
        assert_eq!(
            dec(BinaryParam::Float8, &binary::float8(f64::INFINITY)),
            "Infinity"
        );
        assert_eq!(
            dec(BinaryParam::Float8, &binary::float8(f64::NEG_INFINITY)),
            "-Infinity"
        );
        assert_eq!(dec(BinaryParam::Float8, &binary::float8(-0.0)), "-0");
        assert_eq!(dec(BinaryParam::Float4, &binary::float4(0.25)), "0.25");
        assert_eq!(dec(BinaryParam::Float4, &binary::float4(f32::NAN)), "NaN");
        let sub = f32::from_bits(1);
        let text = dec(BinaryParam::Float4, &binary::float4(sub));
        assert_eq!(text.parse::<f32>().ok(), Some(sub));
    }

    #[test]
    fn bool_nonzero_is_true() {
        assert_eq!(dec(BinaryParam::Bool, &[0]), "f");
        assert_eq!(dec(BinaryParam::Bool, &[1]), "t");
        assert_eq!(dec(BinaryParam::Bool, &[2]), "t");
    }

    #[test]
    fn bytea_and_uuid_become_canonical_text() {
        assert_eq!(dec(BinaryParam::Bytea, &[]), "\\x");
        assert_eq!(dec(BinaryParam::Bytea, &[0, 0xab, 0xff]), "\\x00abff");
        let u: [u8; 16] = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0,
        ];
        assert_eq!(
            dec(BinaryParam::Uuid, &binary::uuid(u)),
            "12345678-9abc-def0-1234-56789abcdef0"
        );
    }

    #[test]
    fn utf8_identity_passes_through() {
        assert_eq!(dec(BinaryParam::Utf8Identity, "あ".as_bytes()), "あ");
    }

    #[test]
    fn wrong_lengths_are_rejected() {
        let cases: [(BinaryParam, usize); 6] = [
            (BinaryParam::Int4, 4),
            (BinaryParam::Int8, 8),
            (BinaryParam::Float4, 4),
            (BinaryParam::Float8, 8),
            (BinaryParam::Bool, 1),
            (BinaryParam::Uuid, 16),
        ];
        for (kind, n) in cases {
            for len in [n - 1, n + 1] {
                assert_eq!(
                    decode_to_text(kind, &vec![0u8; len]),
                    Err(BinaryParamError),
                    "{kind:?} len {len}"
                );
            }
        }
        assert_eq!(
            decode_to_text(BinaryParam::Unsupported, &[]),
            Err(BinaryParamError)
        );
    }
}
