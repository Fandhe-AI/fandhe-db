//! `json_agg`・`jsonb_agg` の確定処理（SQL-57・Issue #3264）。
//!
//! 責務境界: [`crate::sql::array_agg::ArrayAggState::finish`] が並べ替え・重複除去まで終えた
//! 1 次元配列（[`ArrayValue`]）を、JSON 配列テキストへ書き出すだけを担う。累積器の状態・上限・
//! 束縛は `array_agg` と共通で、本モジュールは確定時の写し先（`json` か `jsonb` か）だけが違う。
//! 呼び出し元は `array_agg` の確定処理のみで、結果は `Cell::Json` として SQL 表層・wire へ渡る。
//!
//! 出力規則（PostgreSQL 17 に合わせる）:
//! - `json_agg`: `[` と `]` の間を `, ` で区切る。NULL は `null`。要素の表記は `json_build_object`・
//!   `to_json` と同じ（real/double の非有限値は `"NaN"` 等の文字列、date・timestamp は ISO 8601
//!   の引用文字列、bytea は `\x` 付き 16 進の引用文字列、numeric は位取りを保った数値）
//! - `jsonb_agg`: 空白なしの正規形（`jsonb_build_array`・`to_jsonb` と同じ）。json 要素は
//!   重複キーを後勝ちで畳んでから取り込む。PostgreSQL の jsonb 出力（要素間の空白）との差は
//!   既存の jsonb 出力と同じ既知差分
//!
//! 資源上限（超過は `54000`。確保の前に判定し、部分的な結果は返さない）: 出力全体の長さは
//! [`MAX_JSON_FIELD_LEN`]、入れ子の深さ・要素数は格納時と同じ検証（`validate_json_column_text`・
//! `canonicalize_jsonb_text`）に通す。エスケープによる膨張（最大 6 倍）は、展開後の長さを先に数えて確保の前に止める。

use crate::json::{self, DuplicateKeys, MAX_JSON_FIELD_LEN};
use crate::row_codec::ArrayValue;
use crate::sql::aggregate::accumulator_bug;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::json_fn::push_checked;
use crate::sql::parser::ArrayAggOutput;

const TOO_LARGE_DETAIL: &str = "json aggregate result exceeds the allowed size or nesting depth";

fn too_large() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large(TOO_LARGE_DETAIL)
}

/// 引用符付きの JSON 文字列のエスケープ後の長さ（バイト数）を、文字列を作らずに数える。
/// `json::write_canonical_string` の展開規則（`\"`・`\\`・`\b` 等は 2 バイト、他の制御文字は
/// `\u00xx` の 6 バイト）と一致させる。
fn quoted_len(s: &str) -> usize {
    let mut n: usize = 2;
    for c in s.chars() {
        let add = match c {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        };
        n = n.saturating_add(add);
    }
    n
}

/// 引用符付きの JSON 文字列を `out` へ追記する。エスケープ後の長さを先に数え、既存の `out` の
/// 長さと合わせて出力上限を確保の前に判定する（制御文字の多い入力が 6 倍に膨らんでから拒否する
/// ことを避ける）。確保は必要量だけを `try_reserve` で行い、超過・確保失敗は `54000`。
fn push_quoted(out: &mut String, s: &str) -> Result<(), SqlSurfaceError> {
    if s.len() > MAX_JSON_FIELD_LEN {
        return Err(too_large());
    }
    let need = quoted_len(s);
    let new_len = out.len().checked_add(need).ok_or_else(too_large)?;
    if new_len > MAX_JSON_FIELD_LEN {
        return Err(too_large());
    }
    out.try_reserve(need).map_err(|_| too_large())?;
    json::write_canonical_string(s, out);
    Ok(())
}

/// 非有限の浮動小数は PostgreSQL に合わせて JSON 文字列にする。
fn push_float(out: &mut String, text: &str, finite: bool) -> Result<(), SqlSurfaceError> {
    if finite {
        push_checked(out, text)
    } else {
        push_quoted(out, text)
    }
}

/// 配列の全要素を区切り付きで `out` へ書く（NULL は `null`）。要素ごとの表記は `write`。
fn render_items<T>(
    items: &[Option<T>],
    sep: &str,
    out: &mut String,
    mut write: impl FnMut(&T, &mut String) -> Result<(), SqlSurfaceError>,
) -> Result<(), SqlSurfaceError> {
    push_checked(out, "[")?;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            push_checked(out, sep)?;
        }
        match item {
            None => push_checked(out, "null")?,
            Some(v) => write(v, out)?,
        }
    }
    push_checked(out, "]")
}

/// `array` を JSON 配列テキストへ確定する。`output` が [`ArrayAggOutput::Array`] のときは
/// 呼び出し元が使わない（`array_agg` は配列のまま返す）ため、実装バグとして拒否する。
pub(crate) fn render(
    array: &ArrayValue,
    output: ArrayAggOutput,
) -> Result<String, SqlSurfaceError> {
    let jsonb = match output {
        ArrayAggOutput::Json => false,
        ArrayAggOutput::Jsonb => true,
        ArrayAggOutput::Array => {
            return Err(accumulator_bug("json rendering requested for array_agg"))
        }
    };
    let sep = if jsonb { "," } else { ", " };
    let mut out = String::new();
    match array {
        ArrayValue::Text(v) => render_items(v, sep, &mut out, |s, o| push_quoted(o, s))?,
        ArrayValue::Bool(v) => render_items(v, sep, &mut out, |b, o| {
            push_checked(o, if *b { "true" } else { "false" })
        })?,
        ArrayValue::Integer(v) => {
            render_items(v, sep, &mut out, |n, o| push_checked(o, &n.to_string()))?
        }
        ArrayValue::SmallInt(v) => {
            render_items(v, sep, &mut out, |n, o| push_checked(o, &n.to_string()))?
        }
        ArrayValue::BigInt(v) => {
            render_items(v, sep, &mut out, |n, o| push_checked(o, &n.to_string()))?
        }
        ArrayValue::Real(v) => render_items(v, sep, &mut out, |f, o| {
            push_float(o, &crate::scalar_float::format_real(*f), f.is_finite())
        })?,
        ArrayValue::Double(v) => render_items(v, sep, &mut out, |f, o| {
            push_float(o, &crate::scalar_float::format_double(*f), f.is_finite())
        })?,
        ArrayValue::Numeric(v) => {
            render_items(v, sep, &mut out, |d, o| push_checked(o, &d.to_string()))?
        }
        ArrayValue::Date(v) => render_items(v, sep, &mut out, |d, o| {
            push_quoted(o, &crate::datetime::format_date(*d))
        })?,
        ArrayValue::Timestamp(v) => render_items(v, sep, &mut out, |t, o| {
            push_quoted(o, &json::json_timestamp_text(*t))
        })?,
        ArrayValue::Time(v) => render_items(v, sep, &mut out, |t, o| {
            push_quoted(o, &crate::datetime::format_time(*t))
        })?,
        ArrayValue::Uuid(v) => {
            render_items(v, sep, &mut out, |u, o| push_quoted(o, &u.to_string()))?
        }
        ArrayValue::Bytea(v) => render_items(v, sep, &mut out, |b, o| {
            // 16 進表記は入力の 2 倍。確保の前に長さを見る。
            if b.len().saturating_mul(2) > MAX_JSON_FIELD_LEN {
                return Err(too_large());
            }
            let mut hex = String::new();
            hex.try_reserve(b.len() * 2 + 2).map_err(|_| too_large())?;
            hex.push_str("\\x");
            for byte in b {
                hex.push_str(&format!("{byte:02x}"));
            }
            push_quoted(o, &hex)
        })?,
        ArrayValue::Json(v) => render_items(v, sep, &mut out, |s, o| {
            if jsonb {
                // json → jsonb と同じく重複キーは後勝ちで畳む。
                let value =
                    json::parse_json_with(s, DuplicateKeys::LastWins).map_err(|_| too_large())?;
                let mut canon = String::new();
                json::write_canonical(&value, &mut canon);
                push_checked(o, &canon)
            } else {
                push_checked(o, s)
            }
        })?,
        ArrayValue::Jsonb(v) => render_items(v, sep, &mut out, |s, o| push_checked(o, s))?,
        // 束縛段が拒否する要素型。到達したら束縛層の実装バグ（fail-closed）。
        ArrayValue::Enum(_) | ArrayValue::Multi(_) | ArrayValue::Bit(_) => {
            return Err(accumulator_bug(
                "json aggregate element type reached finish",
            ))
        }
    }
    if jsonb {
        json::canonicalize_jsonb_text(&out).map_err(|_| too_large())
    } else {
        json::validate_json_column_text(&out).map_err(|_| too_large())?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_of(a: ArrayValue) -> String {
        render(&a, ArrayAggOutput::Json).unwrap()
    }

    #[test]
    fn quoted_len_matches_the_escaped_output() {
        for src in ["abc", "a\"b\\c\n\t\u{1}", "日本語\u{1f}", ""] {
            let mut q = String::new();
            json::write_canonical_string(src, &mut q);
            assert_eq!(quoted_len(src), q.len(), "{src:?}");
        }
    }

    #[test]
    fn control_heavy_text_is_rejected_by_expanded_size() {
        // 入力長は上限内だが、U+0001 が 6 倍に展開されると上限を超える。
        let src = "\u{1}".repeat(MAX_JSON_FIELD_LEN / 2);
        let mut out = String::new();
        assert!(push_quoted(&mut out, &src).is_err());
        assert!(out.is_empty());
        assert!(out.capacity() < MAX_JSON_FIELD_LEN);
    }

    #[test]
    fn existing_output_length_counts_toward_the_limit() {
        let mut out = "x".repeat(MAX_JSON_FIELD_LEN - 3);
        assert!(push_quoted(&mut out, "ab").is_err());
        assert_eq!(out.len(), MAX_JSON_FIELD_LEN - 3);
    }

    #[test]
    fn json_uses_comma_space_and_null() {
        assert_eq!(
            json_of(ArrayValue::Integer(vec![Some(1), None, Some(3)])),
            "[1, null, 3]"
        );
    }

    #[test]
    fn jsonb_is_compact() {
        let s = render(
            &ArrayValue::Text(vec![Some("a".into()), None]),
            ArrayAggOutput::Jsonb,
        )
        .unwrap();
        assert_eq!(s, "[\"a\",null]");
    }

    #[test]
    fn text_is_escaped_and_bigint_is_exact() {
        assert_eq!(
            json_of(ArrayValue::Text(vec![Some("a\"b\\c\n".into())])),
            "[\"a\\\"b\\\\c\\n\"]"
        );
        assert_eq!(
            json_of(ArrayValue::BigInt(vec![Some(9_007_199_254_740_993)])),
            "[9007199254740993]"
        );
    }

    #[test]
    fn non_finite_floats_are_strings() {
        assert_eq!(
            json_of(ArrayValue::Double(vec![Some(f64::NAN), Some(1.5)])),
            "[\"NaN\", 1.5]"
        );
    }

    #[test]
    fn bytea_is_hex_string() {
        assert_eq!(
            json_of(ArrayValue::Bytea(vec![Some(vec![0xde, 0xad])])),
            "[\"\\\\xdead\"]"
        );
    }

    #[test]
    fn json_elements_are_embedded_raw_and_jsonb_dedups_keys() {
        let a = ArrayValue::Json(vec![Some("{\"a\": 1, \"a\": 2}".into())]);
        assert_eq!(
            render(&a, ArrayAggOutput::Json).unwrap(),
            "[{\"a\": 1, \"a\": 2}]"
        );
        assert_eq!(render(&a, ArrayAggOutput::Jsonb).unwrap(), "[{\"a\":2}]");
    }

    #[test]
    fn array_output_is_rejected() {
        assert!(render(&ArrayValue::Integer(vec![]), ArrayAggOutput::Array).is_err());
    }
}
