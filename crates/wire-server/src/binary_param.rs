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
    /// int2（2 バイト。TABLE-30・Issue #1561）。
    Int2,
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
    /// time（0 時起点マイクロ秒の 8 バイト BE。TABLE-32・Issue #1571）。
    Time,
    /// timestamptz（2000-01-01 00:00:00 UTC 起点マイクロ秒の 8 バイト BE。TABLE-33・
    /// Issue #1577）。
    TimestampTz,
    /// interval（time i64〔µs〕・day i32・month i32 の 16 バイト BE。TABLE-34・Issue #1583）。
    /// `±infinity` 番兵は engine が受理しないため `OutOfRange`（`22008`）で拒否する。
    Interval,
    /// numeric（`numeric_send` レイアウト。WIRE-26・Issue #2443）。長さ・`ndigits` を
    /// アロケーション前に上限検証し、正規の 10 進テキストへ変換する。
    Numeric,
}

impl BinaryParam {
    /// 範囲外エラー（`22008`）のメッセージに使う型ラベル。受信値は含めず型名のみを返す
    /// （`extended_query` の Bind 処理から呼ばれる。情報漏えい防止のため固定文字列）。
    #[deny(clippy::wildcard_enum_match_arm)]
    pub(crate) fn out_of_range_label(self) -> &'static str {
        match self {
            BinaryParam::TimestampTz => "timestamp",
            BinaryParam::Interval => "interval",
            BinaryParam::Time => "time value",
            BinaryParam::Unsupported
            | BinaryParam::Utf8Identity
            | BinaryParam::Int2
            | BinaryParam::Int4
            | BinaryParam::Int8
            | BinaryParam::Float4
            | BinaryParam::Float8
            | BinaryParam::Bool
            | BinaryParam::Bytea
            | BinaryParam::Uuid
            | BinaryParam::Numeric => "value",
        }
    }
}

/// バイナリ値の復号失敗。長さ不正は `08P01`、TIME・TIMESTAMPTZ・INTERVAL 番兵の範囲外は `22008`、
/// numeric の外部表現不正は `22P03`・範囲超過は `22003`
/// （`extended_query` が別々の `HandlerError` へ写す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryParamError {
    /// 長さが型の受信形式と合わない。
    InvalidLength,
    /// 値が型の許容範囲外（TIME・TIMESTAMPTZ・INTERVAL の ±infinity 番兵。Issue #1571・
    /// #1577・#1583）。
    OutOfRange,
    /// numeric の外部表現が不正（未知の符号・範囲外の `dscale`・範囲外の桁群。`22P03`。
    /// Issue #2443）。
    InvalidRepresentation,
    /// numeric の値が engine の絶対上限（整数部 38 桁・テキスト 1024 バイト）を超える
    /// （`22003`。列ごとの精度超過は engine の束縛段が判定する。Issue #2443）。
    NumericOutOfRange,
}

/// numeric 符号語（`numeric_send` の `sign`）。
const NUMERIC_POS: u16 = 0x0000;
const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;
/// `dscale` に使える下位 14 ビット（PostgreSQL の `NUMERIC_DSCALE_MASK`）。
const NUMERIC_DSCALE_MASK: u16 = 0x3FFF;

/// 先頭から 2 バイトを BE の `u16` として読む（範囲外は長さ不正）。
fn be_u16(raw: &[u8], at: usize) -> Result<u16, BinaryParamError> {
    let end = at.checked_add(2).ok_or(BinaryParamError::InvalidLength)?;
    let b = raw
        .get(at..end)
        .and_then(|s| <[u8; 2]>::try_from(s).ok())
        .ok_or(BinaryParamError::InvalidLength)?;
    Ok(u16::from_be_bytes(b))
}

/// 0..=9999 の 1 群を 4 桁（`pad` なら先頭ゼロ込み、でなければ先頭ゼロ省略）で `out` へ追記する。
/// 呼び出し側が `g <= 9999` を検証済みの契約（桁は 0..=9 のため `b'0' + d` は溢れない）。
fn push_group(out: &mut String, g: u16, pad: bool, nonzero: &mut bool) {
    let digits = [g / 1000, g / 100 % 10, g / 10 % 10, g % 10];
    let mut started = pad;
    for d in digits {
        if d != 0 {
            started = true;
            *nonzero = true;
        }
        if started {
            out.push(char::from(
                b'0'.saturating_add(u8::try_from(d).unwrap_or(0)),
            ));
        }
    }
    if !started {
        // 先頭群が 0 の場合（整数部 0）は呼び出し側が扱うため通常到達しない。
        out.push('0');
    }
}

/// numeric のバイナリ受信形式を正規 10 進テキスト（または `NaN`／`Infinity`／`-Infinity`）へ
/// 復号する（WIRE-26・Issue #2443）。受信データ経路のため添字・`unwrap` を使わず、
/// 長さ・`ndigits`・符号・`dscale`・桁群・出力長をすべてアロケーション前に検査する。
/// 判定順: 長さ（08P01）→ 符号・`dscale`・桁群（22P03）→ 範囲（22003）。
/// 小数第 `dscale` 位より下の桁は PostgreSQL の受信処理と同じく切り捨て、列の精度・scale への
/// 丸めと超過判定は engine の束縛段（`parse_for_column`）に任せる。
fn decode_numeric(raw: &[u8]) -> Result<String, BinaryParamError> {
    if raw.len() < 8 {
        return Err(BinaryParamError::InvalidLength);
    }
    let ndigits = usize::from(be_u16(raw, 0)?);
    if ndigits > crate::result_encoder::binary::NUMERIC_MAX_GROUPS {
        return Err(BinaryParamError::InvalidLength);
    }
    let expected = ndigits
        .checked_mul(2)
        .and_then(|n| n.checked_add(8))
        .ok_or(BinaryParamError::InvalidLength)?;
    if raw.len() != expected {
        return Err(BinaryParamError::InvalidLength);
    }
    let weight = i32::from(be_u16(raw, 2)? as i16);
    let sign = be_u16(raw, 4)?;
    let dscale = be_u16(raw, 6)?;
    if !matches!(
        sign,
        NUMERIC_POS | NUMERIC_NEG | NUMERIC_NAN | NUMERIC_PINF | NUMERIC_NINF
    ) || dscale & !NUMERIC_DSCALE_MASK != 0
    {
        return Err(BinaryParamError::InvalidRepresentation);
    }
    let mut groups = [0u16; crate::result_encoder::binary::NUMERIC_MAX_GROUPS];
    for (i, slot) in groups.iter_mut().take(ndigits).enumerate() {
        let at = i
            .checked_mul(2)
            .and_then(|n| n.checked_add(8))
            .ok_or(BinaryParamError::InvalidLength)?;
        let g = be_u16(raw, at)?;
        if g > 9999 {
            return Err(BinaryParamError::InvalidRepresentation);
        }
        *slot = g;
    }
    match sign {
        NUMERIC_NAN => return Ok("NaN".to_string()),
        NUMERIC_PINF => return Ok("Infinity".to_string()),
        NUMERIC_NINF => return Ok("-Infinity".to_string()),
        _ => {}
    }
    // 指数 `e`（10^(4e) の位）の群。範囲外は 0。
    let group_at = |e: i32| -> u16 {
        weight
            .checked_sub(e)
            .and_then(|i| usize::try_from(i).ok())
            .filter(|i| *i < ndigits)
            .and_then(|i| groups.get(i))
            .copied()
            .unwrap_or(0)
    };
    // 先頭の非ゼロ群から整数部の有効桁数を求める（全群 0 なら整数部は `0`）。
    let first = groups.iter().take(ndigits).position(|g| *g != 0);
    let int_top_exp = first.and_then(|f| {
        let e = weight.checked_sub(i32::try_from(f).ok()?)?;
        (e >= 0).then_some(e)
    });
    let int_digits = match (int_top_exp, first.and_then(|f| groups.get(f)).copied()) {
        (Some(e), Some(g)) => {
            let lead = match g {
                0..=9 => 1,
                10..=99 => 2,
                100..=999 => 3,
                _ => 4,
            };
            e.checked_mul(4)
                .and_then(|n| n.checked_add(lead))
                .ok_or(BinaryParamError::NumericOutOfRange)?
        }
        _ => 1,
    };
    if int_digits > i32::from(engine::numeric::MAX_PRECISION) {
        return Err(BinaryParamError::NumericOutOfRange);
    }
    let scale = usize::from(dscale);
    let total = usize::try_from(int_digits)
        .ok()
        .and_then(|n| n.checked_add(1)) // 符号
        .and_then(|n| n.checked_add(if scale > 0 { scale.checked_add(1)? } else { 0 }))
        .ok_or(BinaryParamError::NumericOutOfRange)?;
    if total > engine::numeric::MAX_LITERAL_LEN {
        return Err(BinaryParamError::NumericOutOfRange);
    }
    let mut out = String::with_capacity(total);
    let mut nonzero = false;
    match int_top_exp {
        Some(top) => {
            for e in (0..=top).rev() {
                push_group(&mut out, group_at(e), e != top, &mut nonzero);
            }
        }
        None => out.push('0'),
    }
    if scale > 0 {
        out.push('.');
        let mut remaining = scale;
        let mut e: i32 = -1;
        while remaining > 0 {
            let g = group_at(e);
            // 4 桁を固定長配列で生成し、ヒープ確保なしで必要桁だけ out へ追記する。
            let digits = [g / 1000, g / 100 % 10, g / 10 % 10, g % 10];
            let take = remaining.min(4);
            for d in digits.iter().take(take) {
                if *d != 0 {
                    nonzero = true;
                }
                out.push(char::from(
                    b'0'.saturating_add(u8::try_from(*d).unwrap_or(0)),
                ));
            }
            remaining -= take;
            e = e.saturating_sub(1);
        }
    }
    // 切り捨て後に全桁 0 なら符号を付けない（`-0` を出さない）。
    if sign == NUMERIC_NEG && nonzero {
        out.insert(0, '-');
    }
    Ok(out)
}

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
        BinaryParam::Unsupported => return Err(BinaryParamError::InvalidLength),
        BinaryParam::Utf8Identity => return Ok(raw.to_vec()),
        BinaryParam::Int2 => {
            let b = <[u8; 2]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            i16::from_be_bytes(b).to_string()
        }
        BinaryParam::Int4 => {
            let b = <[u8; 4]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            i32::from_be_bytes(b).to_string()
        }
        BinaryParam::Int8 => {
            let b = <[u8; 8]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            i64::from_be_bytes(b).to_string()
        }
        BinaryParam::Float4 => {
            let b = <[u8; 4]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            let v = f32::from_bits(u32::from_be_bytes(b));
            float_text(&v, v.is_nan(), v.is_infinite(), v.is_sign_negative())
        }
        BinaryParam::Float8 => {
            let b = <[u8; 8]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            let v = f64::from_bits(u64::from_be_bytes(b));
            float_text(&v, v.is_nan(), v.is_infinite(), v.is_sign_negative())
        }
        BinaryParam::Bool => {
            // PostgreSQL の `boolrecv` と同じく 0 以外は真。
            let [b] = <[u8; 1]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            if b != 0 { "t" } else { "f" }.to_string()
        }
        BinaryParam::Uuid => {
            let b = <[u8; 16]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            engine::uuid::Uuid::from_bytes(b).to_string()
        }
        BinaryParam::Numeric => decode_numeric(raw)?,
        BinaryParam::Bytea => engine::bytea::format_hex_text(raw),
        BinaryParam::Time => {
            let b = <[u8; 8]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            let v = i64::from_be_bytes(b);
            // `format_time` は範囲外を丸めるため、整形前に必ず範囲を検査する（fail-closed）。
            if !engine::datetime::validate_time_micros(v) {
                return Err(BinaryParamError::OutOfRange);
            }
            engine::datetime::format_time(v)
        }
        BinaryParam::Interval => {
            let b = <[u8; 16]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            // 固定長 16 バイトを添字なしで micros(8)・days(4)・months(4) に分解する。
            let (micros_b, rest) = b
                .split_first_chunk::<8>()
                .ok_or(BinaryParamError::InvalidLength)?;
            let (days_b, months_b) = rest
                .split_first_chunk::<4>()
                .ok_or(BinaryParamError::InvalidLength)?;
            let months_b =
                <[u8; 4]>::try_from(months_b).map_err(|_| BinaryParamError::InvalidLength)?;
            let v = engine::datetime::Interval {
                months: i32::from_be_bytes(months_b),
                days: i32::from_be_bytes(*days_b),
                micros: i64::from_be_bytes(*micros_b),
            };
            // ±infinity 番兵は engine が受理しないため fail-closed。
            if !engine::datetime::validate_interval(&v) {
                return Err(BinaryParamError::OutOfRange);
            }
            // engine の正規テキスト（IntervalStyle=postgres）を束縛経路へ渡す。
            // `parse_interval` との往復は engine 側で保証済み（バイナリ ≡ テキスト）。
            engine::datetime::format_interval(&v)
        }
        BinaryParam::TimestampTz => {
            let b = <[u8; 8]>::try_from(raw).map_err(|_| BinaryParamError::InvalidLength)?;
            // untrusted 入力: PG epoch 加算は checked、続けて engine の範囲検査を通す。
            // PG の infinity 番兵（i64::MAX／MIN）も範囲外として fail-closed で拒否する。
            let v = i64::from_be_bytes(b)
                .checked_add(crate::result_encoder::binary::PG_EPOCH_UNIX_MICROS)
                .filter(|v| engine::datetime::validate_timestamptz_micros(*v))
                .ok_or(BinaryParamError::OutOfRange)?;
            // 必ず明示 offset（`+00`）付きで整形する。offset 無しのテキストは engine がセッション
            // TimeZone の壁時計として解釈するため（#1575）、バイナリ値の意味をセッション TZ から
            // 独立に保つ。
            engine::datetime::format_timestamptz(v, &engine::datetime::SessionTimeZone::utc())
        }
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
        for v in [0, 1, -1, i16::MIN, i16::MAX] {
            assert_eq!(dec(BinaryParam::Int2, &binary::int2(v)), v.to_string());
        }
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

    /// TIME は 0 時起点 µs の 8 バイト BE。境界値は `parse_time` で元の値へ戻り、
    /// 範囲外は丸めずに `OutOfRange` で拒否する（Issue #1571）。
    #[test]
    fn time_round_trips_and_rejects_out_of_range() {
        for v in [0i64, 1, 86_399_999_999, 86_400_000_000] {
            let t = dec(BinaryParam::Time, &binary::time(v));
            assert_eq!(engine::datetime::parse_time(&t).ok(), Some(v), "{t}");
        }
        for v in [-1i64, 86_400_000_001, i64::MIN, i64::MAX] {
            assert_eq!(
                decode_to_text(BinaryParam::Time, &binary::time(v)),
                Err(BinaryParamError::OutOfRange),
                "{v}"
            );
        }
    }

    /// TIMESTAMPTZ は 2000-01-01 UTC 起点 µs の 8 バイト BE。境界値は `parse_timestamptz` で
    /// 元の UTC µs へ戻り、範囲外・infinity 番兵は `OutOfRange`（Issue #1577）。
    #[test]
    fn timestamptz_round_trips_and_rejects_out_of_range() {
        use engine::datetime::{
            parse_timestamptz, SessionTimeZone, TIMESTAMPTZ_MAX_MICROS, TIMESTAMPTZ_MIN_MICROS,
        };
        let pg = binary::PG_EPOCH_UNIX_MICROS;
        for unix in [
            TIMESTAMPTZ_MIN_MICROS,
            TIMESTAMPTZ_MAX_MICROS,
            pg,
            pg - 1,
            pg + 123_456,
            0,
        ] {
            let raw = (unix - pg).to_be_bytes();
            let t = dec(BinaryParam::TimestampTz, &raw);
            assert_eq!(
                parse_timestamptz(&t, &SessionTimeZone::utc()).ok(),
                Some(unix),
                "{t}"
            );
        }
        for pgv in [
            i64::MAX,
            i64::MIN,
            TIMESTAMPTZ_MAX_MICROS - pg + 1,
            TIMESTAMPTZ_MIN_MICROS - pg - 1,
        ] {
            assert_eq!(
                decode_to_text(BinaryParam::TimestampTz, &pgv.to_be_bytes()),
                Err(BinaryParamError::OutOfRange),
                "{pgv}"
            );
        }
    }

    /// INTERVAL は time i64・day i32・month i32 の 16 バイト BE。番兵以外の格子は
    /// `parse_interval` で元の値へ往復し、固定バイト列と一致する。番兵 2 種は `OutOfRange`
    /// （Issue #1583）。
    #[test]
    fn interval_round_trips_and_rejects_sentinels() {
        use engine::datetime::{parse_interval, validate_interval, Interval};
        for months in [0, 1, -1, 12, -13, i32::MAX, i32::MIN, i32::MIN + 1] {
            for days in [0, 1, -1, 30, i32::MAX, i32::MIN] {
                for micros in [0, 1, -1, 3_600_000_000, -86_399_999_999, i64::MAX, i64::MIN] {
                    let v = Interval {
                        months,
                        days,
                        micros,
                    };
                    let raw = binary::interval(&v);
                    let mut hand = Vec::new();
                    hand.extend_from_slice(&micros.to_be_bytes());
                    hand.extend_from_slice(&days.to_be_bytes());
                    hand.extend_from_slice(&months.to_be_bytes());
                    assert_eq!(raw.as_slice(), hand.as_slice());
                    if !validate_interval(&v) {
                        assert_eq!(
                            decode_to_text(BinaryParam::Interval, &raw),
                            Err(BinaryParamError::OutOfRange)
                        );
                        continue;
                    }
                    let t = dec(BinaryParam::Interval, &raw);
                    assert_eq!(parse_interval(&t).ok(), Some(v), "{t}");
                }
            }
        }
        // 番兵 2 種（全 0x7F/0xFF 側・全 0x80/0x00 側）は明示的に拒否する。
        let mut max = vec![0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        max.extend_from_slice(&[0x7F, 0xFF, 0xFF, 0xFF, 0x7F, 0xFF, 0xFF, 0xFF]);
        let mut min = vec![0x80, 0, 0, 0, 0, 0, 0, 0];
        min.extend_from_slice(&[0x80, 0, 0, 0, 0x80, 0, 0, 0]);
        for raw in [max, min] {
            assert_eq!(
                decode_to_text(BinaryParam::Interval, &raw),
                Err(BinaryParamError::OutOfRange)
            );
        }
        for len in [0, 15, 17, 32] {
            assert_eq!(
                decode_to_text(BinaryParam::Interval, &vec![0u8; len]),
                Err(BinaryParamError::InvalidLength)
            );
        }
    }

    /// 任意の 16 バイトで panic せず、成功時は `parse_interval` で往復するテキストになる。
    #[test]
    fn interval_arbitrary_bytes_never_panic() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..5000 {
            let mut raw = [0u8; 16];
            for b in raw.iter_mut() {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                *b = (x >> 56) as u8;
            }
            match decode_to_text(BinaryParam::Interval, &raw) {
                Ok(t) => {
                    let s = String::from_utf8(t).expect("utf8");
                    assert!(engine::datetime::parse_interval(&s).is_ok(), "{s}");
                }
                Err(e) => assert_eq!(e, BinaryParamError::OutOfRange),
            }
        }
    }

    #[test]
    fn utf8_identity_passes_through() {
        assert_eq!(dec(BinaryParam::Utf8Identity, "あ".as_bytes()), "あ");
    }

    #[test]
    fn wrong_lengths_are_rejected() {
        let cases: [(BinaryParam, usize); 10] = [
            (BinaryParam::Int2, 2),
            (BinaryParam::Int4, 4),
            (BinaryParam::Int8, 8),
            (BinaryParam::Float4, 4),
            (BinaryParam::Float8, 8),
            (BinaryParam::Bool, 1),
            (BinaryParam::Uuid, 16),
            (BinaryParam::Time, 8),
            (BinaryParam::TimestampTz, 8),
            (BinaryParam::Interval, 16),
        ];
        for (kind, n) in cases {
            for len in [n - 1, n + 1] {
                assert_eq!(
                    decode_to_text(kind, &vec![0u8; len]),
                    Err(BinaryParamError::InvalidLength),
                    "{kind:?} len {len}"
                );
            }
        }
        assert_eq!(
            decode_to_text(BinaryParam::Unsupported, &[]),
            Err(BinaryParamError::InvalidLength)
        );
    }

    /// numeric バイト列を組み立てる（ヘッダ + 桁群。値の正規性は検査しない）。
    fn nraw(ndigits: u16, weight: i16, sign: u16, dscale: u16, groups: &[u16]) -> Vec<u8> {
        let mut v = Vec::new();
        for w in [ndigits, weight as u16, sign, dscale] {
            v.extend_from_slice(&w.to_be_bytes());
        }
        for g in groups {
            v.extend_from_slice(&g.to_be_bytes());
        }
        v
    }

    fn ndec(raw: &[u8]) -> Result<String, BinaryParamError> {
        decode_to_text(BinaryParam::Numeric, raw).map(|b| String::from_utf8(b).expect("utf8"))
    }

    #[test]
    fn numeric_round_trips_with_result_encoder() {
        let max38 = 10u128.pow(38) - 1;
        let cases: [(bool, u128, u8); 13] = [
            (false, 0, 0),
            (false, 0, 2),
            (false, 1, 0),
            (true, 1, 0),
            (false, 9999, 0),
            (false, 10000, 0),
            (true, 5, 1),
            (false, 1_234_567_890_123, 4),
            (false, max38, 0),
            (true, max38, 0),
            (false, 1, 38),
            (false, 1, 37),
            (false, max38, 38),
        ];
        for (neg, mag, scale) in cases {
            let (buf, n) = binary::numeric(neg, mag, scale).expect("encode");
            let raw = buf.get(..n).expect("len");
            let signed = i128::try_from(mag).expect("fits");
            let d = engine::numeric::Decimal::from_parts(if neg { -signed } else { signed }, scale)
                .expect("decimal");
            assert_eq!(
                ndec(raw).expect("decode"),
                d.to_string(),
                "{neg} {mag} {scale}"
            );
        }
    }

    #[test]
    fn numeric_normalizes_and_truncates() {
        // 先頭・末尾のゼロ群を含む 12.5（weight 1: [0, 12, 5000, 0]）。
        assert_eq!(
            ndec(&nraw(4, 1, 0, 1, &[0, 12, 5000, 0])).as_deref(),
            Ok("12.5")
        );
        // dscale より下の桁は切り捨て（123.456 → dscale 1）。
        assert_eq!(
            ndec(&nraw(2, 0, 0, 1, &[123, 4560])).as_deref(),
            Ok("123.4")
        );
        // 切り捨てで全桁 0 になる負値は符号なし。
        assert_eq!(
            ndec(&nraw(1, -1, NUMERIC_NEG, 2, &[10])).as_deref(),
            Ok("0.00")
        );
        assert_eq!(
            ndec(&nraw(1, -1, NUMERIC_NEG, 2, &[1000])).as_deref(),
            Ok("-0.10")
        );
        // 負号付きのゼロ。
        assert_eq!(ndec(&nraw(0, 0, NUMERIC_NEG, 0, &[])).as_deref(), Ok("0"));
        // 全群ゼロなら巨大な weight でも 0。
        assert_eq!(ndec(&nraw(1, i16::MAX, 0, 0, &[0])).as_deref(), Ok("0"));
        // dscale 0 は小数部を捨てる。
        assert_eq!(ndec(&nraw(2, 0, 0, 0, &[7, 9999])).as_deref(), Ok("7"));
        // weight が桁群より大きい場合はゼロ詰め。
        assert_eq!(ndec(&nraw(1, 2, 0, 0, &[3])).as_deref(), Ok("300000000"));
    }

    #[test]
    fn numeric_special_values() {
        assert_eq!(ndec(&nraw(0, 0, NUMERIC_NAN, 0, &[])).as_deref(), Ok("NaN"));
        assert_eq!(
            ndec(&nraw(0, 0, NUMERIC_PINF, 0, &[])).as_deref(),
            Ok("Infinity")
        );
        assert_eq!(
            ndec(&nraw(0, 0, NUMERIC_NINF, 0, &[])).as_deref(),
            Ok("-Infinity")
        );
        // 特殊値でも桁群は検証する。
        assert_eq!(
            ndec(&nraw(1, 0, NUMERIC_NAN, 0, &[10000])),
            Err(BinaryParamError::InvalidRepresentation)
        );
    }

    // PG17 実測: 末尾の余分なバイトは 22P03、ndigits 33 以上は受理。本実装は WIRE-26 に従い
    // 08P01 で拒否する意図的な差分（照合表は tests/wire26_numeric_pg17_vectors.rs。Issue #2444）。
    #[test]
    fn numeric_length_errors_are_invalid_length() {
        for len in 0..8 {
            assert_eq!(ndec(&vec![0u8; len]), Err(BinaryParamError::InvalidLength));
        }
        let ok = nraw(2, 0, 0, 0, &[1, 2]);
        assert!(ndec(&ok).is_ok());
        for cut in [1usize, 2] {
            assert_eq!(
                ndec(ok.get(..ok.len() - cut).expect("cut")),
                Err(BinaryParamError::InvalidLength)
            );
            let mut longer = ok.clone();
            longer.extend(std::iter::repeat_n(0u8, cut));
            assert_eq!(ndec(&longer), Err(BinaryParamError::InvalidLength));
        }
        // ndigits の上限は 32（受理）・33（拒否）。
        let g32 = vec![0u16; 32];
        assert!(ndec(&nraw(32, 0, 0, 0, &g32)).is_ok());
        let g33 = vec![0u16; 33];
        assert_eq!(
            ndec(&nraw(33, 0, 0, 0, &g33)),
            Err(BinaryParamError::InvalidLength)
        );
        assert_eq!(
            ndec(&nraw(0xFFFF, 0, 0, 0, &[])),
            Err(BinaryParamError::InvalidLength)
        );
    }

    #[test]
    fn numeric_representation_errors_are_22p03() {
        let bad = BinaryParamError::InvalidRepresentation;
        for sign in [0x8000u16, 0x0001, 0xFFFF] {
            assert_eq!(ndec(&nraw(0, 0, sign, 0, &[])), Err(bad), "sign {sign:#x}");
        }
        for dscale in [0x4000u16, 0xFFFF] {
            assert_eq!(
                ndec(&nraw(0, 0, 0, dscale, &[])),
                Err(bad),
                "dscale {dscale:#x}"
            );
        }
        // 0x3FFF は 22P03 ではなく範囲超過（22003）。
        assert_eq!(
            ndec(&nraw(0, 0, 0, 0x3FFF, &[])),
            Err(BinaryParamError::NumericOutOfRange)
        );
        for g in [10000u16, 0xFFFF] {
            assert_eq!(ndec(&nraw(1, 0, 0, 0, &[g])), Err(bad), "group {g}");
        }
        // 長さ不正が表現不正より先。
        assert_eq!(
            ndec(&nraw(1, 0, 0x8000, 0, &[])),
            Err(BinaryParamError::InvalidLength)
        );
        // 符号不正と桁不正が同時なら 22P03。
        assert_eq!(ndec(&nraw(1, 0, 0x8000, 0, &[10000])), Err(bad));
    }

    #[test]
    fn numeric_range_errors_are_22003() {
        let over = BinaryParamError::NumericOutOfRange;
        // 整数部 38 桁は受理、39 桁は拒否（weight 9・先頭群 1 桁 = 37 桁、先頭群 2 桁 = 38 桁）。
        let ok38 = ndec(&nraw(1, 9, 0, 0, &[10])).expect("38 digits");
        assert_eq!(ok38.len(), 38);
        assert_eq!(ndec(&nraw(1, 9, 0, 0, &[100])), Err(over));
        assert_eq!(ndec(&nraw(1, i16::MAX, 0, 0, &[1])), Err(over));
        // テキスト長（符号 1 + 整数部 1 + '.' + dscale）が 1024 ちょうど（dscale 1021）は受理、超過は拒否。
        assert!(ndec(&nraw(0, 0, 0, 1021, &[])).is_ok());
        assert_eq!(ndec(&nraw(0, 0, 0, 1022, &[])), Err(over));
    }
}
