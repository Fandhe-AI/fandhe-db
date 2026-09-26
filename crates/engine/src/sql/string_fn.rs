//! 文字列スカラー関数群の純粋な実装（Issue #919、対象ビヘイビア: SQL-26
//! （検討中）・ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-26・
//! `docs/spec/05-tasks.md` TASK-210）。
//!
//! 責務境界: 本モジュールは行コンテキスト・`ExprValue`・エラー型
//! （`SqlSurfaceError` 以外）に依存しない純粋関数のみを提供する。NULL 伝播・
//! 関数解決・組み込み名の予約は `sql::udf_call`（[`crate::sql::udf_call::apply_builtin`]）
//! が担う（本モジュールは非 NULL の `&str` 入力のみを受け取る）。
//!
//! # 文字単位（AC3）
//!
//! 位置・長さはすべて **Unicode スカラー値（Rust の `char`）単位**で扱う
//! （書記素クラスタ単位ではない）。バイト単位を返す関数（`octet_length` 等）は
//! 本 Issue のスコープ外。行ストアの `TEXT` 値は永続化時点で UTF-8 検証済み
//! （`crate::row_codec`）のため、不正な UTF-8 はこの層には到達しない。
//!
//! # PostgreSQL との既知の差
//!
//! - `lower`/`upper` は Rust 標準ライブラリの Unicode 既定変換
//!   （`str::to_lowercase`/`to_uppercase`）を使う。libc ロケール依存の
//!   PostgreSQL とは `ß` → `SS` のように結果が変わる文字がありうる。
//! - `trim` は半角空白 `' '` のみを除去する（PostgreSQL の `trim(both ' ' from s)`
//!   既定と同じで、タブ・改行・全角空白は対象外）。
//! - TEXT 同士の比較はバイト順（UTF-8 のコードポイント順）で、PostgreSQL の
//!   `"C"` 照合順に相当する。
//!
//! untrusted な行データ・SQL リテラルを扱うため `unwrap`/`expect`/添字アクセス
//! `[]` を使わない（`.claude/rules/coding-rust.md`）。

use crate::row_codec::MAX_TEXT_FIELD_LEN;
use crate::sql::allowlist::SqlSurfaceError;

/// 文字列関数の結果 1 件あたりのバイト長上限。行の `TEXT` 列値 1 個分の上限
/// （[`MAX_TEXT_FIELD_LEN`]）と同値を採用する（`REPLACE`/`CONCAT`/`UPPER` は
/// 入力より結果が伸びうるため、この上限を超えたら `54000` で fail-closed に
/// 拒否する。security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
const MAX_RESULT_LEN: usize = MAX_TEXT_FIELD_LEN as usize;

/// 構築済み文字列が [`MAX_RESULT_LEN`] を超えていないか検査する共通ヘルパー。
fn check_result_len(s: String) -> Result<String, SqlSurfaceError> {
    if s.len() > MAX_RESULT_LEN {
        return Err(SqlSurfaceError::payload_too_large(
            "string function result exceeds the maximum TEXT field length",
        ));
    }
    Ok(s)
}

/// `LOWER(s)`: Unicode 既定の小文字変換（フル大小文字変換。ロケール非依存で
/// 決定的）。
pub(crate) fn lower(s: &str) -> Result<String, SqlSurfaceError> {
    check_result_len(s.to_lowercase())
}

/// `UPPER(s)`: Unicode 既定の大文字変換。
pub(crate) fn upper(s: &str) -> Result<String, SqlSurfaceError> {
    check_result_len(s.to_uppercase())
}

/// `LENGTH(s)`: 文字数（Unicode スカラー値単位）。`f64` として返すが、
/// 1 列あたりの上限（[`MAX_TEXT_FIELD_LEN`]）が `f64` の正確表現域
/// （`2^53`）を大きく下回るため精度は失われない。
pub(crate) fn length(s: &str) -> f64 {
    s.chars().count() as f64
}

/// 引数（`start`/`len`）を `i64` へ検証つきで変換する（非有限・非整数・
/// `i64` 範囲外はすべて `22000`）。
fn arg_as_i64(v: f64, what: &str) -> Result<i64, SqlSurfaceError> {
    if !v.is_finite() {
        return Err(SqlSurfaceError::invalid_input(format!(
            "{what} must be a finite number"
        )));
    }
    if v.fract() != 0.0 {
        return Err(SqlSurfaceError::invalid_input(format!(
            "{what} must be an integer"
        )));
    }
    // `as i64` は範囲外の `f64` を丸めて `i64::MIN`/`MAX` へ飽和させるため
    // （Rust 1.45 以降の既定キャスト仕様）、事前に範囲を検査してから変換する
    // （黙った飽和変換で異なる値を同一視しないため）。
    //
    // codex-review P1 指摘対応: `i64::MAX`（`2^63 - 1`）は `f64` の 53 bit 仮数部で
    // 正確に表現できず、`i64::MAX as f64` は丸めにより `2^63` になる。そのため
    // 旧実装の `v > (i64::MAX as f64)` は `v == 9223372036854775808.0`（`2^63`）を
    // 上限超過として拒否できず、後続の `v as i64` が `i64::MAX` へ飽和して範囲外
    // 引数を誤って受理していた（`22000` 拒否契約違反）。`i64::MIN`（`-2^63`）は
    // 2 の冪で `f64` に正確に表現できるため、上限は `-(i64::MIN as f64)`
    // （`2^63` を正確な値として算出）を使い、下限はそのまま `i64::MIN as f64` と
    // 比較する（`sql::group_by::cmp_signed_to_literal` と同じ境界値の考え方）。
    if v < (i64::MIN as f64) || v >= -(i64::MIN as f64) {
        return Err(SqlSurfaceError::invalid_input(format!(
            "{what} is out of range"
        )));
    }
    Ok(v as i64)
}

/// `SUBSTR(s, start[, len])`: 1 始まりの文字位置。`[start, start+len)` と
/// `[1, 文字数]` の交差を返す（PostgreSQL の `substr`/`substring` と同じ窓計算。
/// `start` が 0 以下でも `len` 分の窓で切り、負の `len` は `22000` で拒否する）。
pub(crate) fn substr(s: &str, start: f64, len: Option<f64>) -> Result<String, SqlSurfaceError> {
    let chars: Vec<char> = s.chars().collect();
    let total = chars.len() as i64;
    let start_i = arg_as_i64(start, "substr start")?;
    // PostgreSQL の窓計算: 論理区間は [start, start+len) だが 1 始まりなので
    // 文字配列上のオフセットは (start-1) を基準にする。`checked_*` で
    // オーバーフローを未定義動作にしない（coding-rust.md「untrusted 入力の扱い」）。
    let (from, to): (i64, i64) = match len {
        None => (start_i, total.saturating_add(1)),
        Some(len_raw) => {
            let len_i = arg_as_i64(len_raw, "substr length")?;
            if len_i < 0 {
                return Err(SqlSurfaceError::invalid_input(
                    "substr length must not be negative",
                ));
            }
            let end = start_i
                .checked_add(len_i)
                .ok_or_else(|| SqlSurfaceError::invalid_input("substr start + length overflows"))?;
            (start_i, end)
        }
    };
    let from_clamped = from.max(1);
    let to_clamped = to.min(total.saturating_add(1)).max(from_clamped);
    if from_clamped > total {
        return check_result_len(String::new());
    }
    // 1 始まりの論理位置を 0 始まりの `Vec<char>` 添字へ変換する。
    let start_idx = usize::try_from(from_clamped - 1).unwrap_or(0);
    let end_idx = usize::try_from(to_clamped - 1)
        .unwrap_or(0)
        .min(chars.len());
    if start_idx >= end_idx {
        return check_result_len(String::new());
    }
    let slice = chars
        .get(start_idx..end_idx)
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "substr window computation produced an out-of-range slice".to_string(),
        })?;
    check_result_len(slice.iter().collect())
}

/// `CONCAT` の 2 引数畳み込み単位（可変長 `CONCAT(a,b,c,...)` は束縛段
/// （`sql::udf_call::bind_call`）でこの 2 引数関数の左畳み込みへ展開する）。
/// NULL 引数は空文字として扱うため（`ExprValue::Null` を渡された時点で呼び出し元
/// が既に空文字へ写像済み）、本関数自体は NULL を扱わない。
pub(crate) fn concat2(a: &str, b: &str) -> Result<String, SqlSurfaceError> {
    let mut out = String::new();
    out.try_reserve(a.len().saturating_add(b.len()))
        .map_err(|_| {
            SqlSurfaceError::payload_too_large("concat result exceeds available memory")
        })?;
    out.push_str(a);
    out.push_str(b);
    check_result_len(out)
}

/// `TRIM(s)`: 先頭・末尾の半角空白 `' '` のみを除去する（PostgreSQL の既定。
/// タブ・改行・全角空白は対象外）。
pub(crate) fn trim(s: &str) -> Result<String, SqlSurfaceError> {
    check_result_len(s.trim_matches(' ').to_string())
}

/// `REPLACE(s, from, to)`: 部分文字列をすべて置換する。`from` が空文字なら
/// `s` をそのまま返す（無限ループ・空文字区切りでの異常な膨張を避ける。
/// `str::replace` は空パターンで各文字境界に `to` を挿入してしまうため、
/// ここで明示的に素通しへ倒す）。
///
/// `s`／`from`／`to` はそれぞれ個別には [`MAX_RESULT_LEN`] 以下でも、`from` が
/// 短く `to` が長い場合（例: 1 バイトの `from` を大量に含む `s` を巨大な `to`
/// へ置換）は出現回数に比例して結果が乗算的に膨張しうる。`String::replace` は
/// 置換後の文字列をその場で構築するため、`check_result_len` に到達する前に
/// 巨大なアロケーションが発生してしまう（security.md「不安全な設計｜無制限
/// リソース確保（DoS）」対応）。`str::matches` によるカウントは追加確保を伴わない
/// ため、確保前に `checked_*` 演算で最終長を見積もり、上限超過を先に検査する
/// （coding-rust.md「untrusted 入力の扱い」）。
pub(crate) fn replace(s: &str, from: &str, to: &str) -> Result<String, SqlSurfaceError> {
    if from.is_empty() {
        return check_result_len(s.to_string());
    }
    let occurrences = s.matches(from).count();
    let removed = occurrences
        .checked_mul(from.len())
        .ok_or_else(replace_overflow)?;
    let added = occurrences
        .checked_mul(to.len())
        .ok_or_else(replace_overflow)?;
    let estimated_len = s
        .len()
        .checked_sub(removed)
        .ok_or_else(replace_overflow)?
        .checked_add(added)
        .ok_or_else(replace_overflow)?;
    if estimated_len > MAX_RESULT_LEN {
        return Err(SqlSurfaceError::payload_too_large(
            "string function result exceeds the maximum TEXT field length",
        ));
    }
    let out = s.replace(from, to);
    check_result_len(out)
}

/// [`replace`] の見積り計算がオーバーフローした場合の fail-closed エラー
/// （到達し得ても [`MAX_RESULT_LEN`] 超過と同じ拒否として扱う）。
fn replace_overflow() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large(
        "string function result exceeds the maximum TEXT field length",
    )
}

/// `POSITION(needle IN haystack)`: 1 始まりの文字位置。見つからなければ 0、
/// `needle` が空文字なら 1（PostgreSQL と同じ）。
pub(crate) fn position(haystack: &str, needle: &str) -> f64 {
    if needle.is_empty() {
        return 1.0;
    }
    match haystack.find(needle) {
        None => 0.0,
        Some(byte_idx) => {
            // バイトオフセットを文字オフセット（1 始まり）へ変換する。
            let char_idx = haystack[..byte_idx].chars().count();
            (char_idx + 1) as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_upper_roundtrip_ascii() {
        assert_eq!(lower("ABC").unwrap(), "abc");
        assert_eq!(upper("abc").unwrap(), "ABC");
    }

    #[test]
    fn length_counts_characters_not_bytes_for_multibyte_text() {
        // 日本語 3 文字（バイト長は 9）。
        assert_eq!(length("あいう"), 3.0);
        // 絵文字（結合文字を含まない単純な例）も 1 文字として数える。
        assert_eq!(length("😀"), 1.0);
    }

    #[test]
    fn substr_matches_postgres_window_semantics() {
        assert_eq!(substr("hello", 2.0, Some(3.0)).unwrap(), "ell");
        assert_eq!(substr("hello", 1.0, None).unwrap(), "hello");
        // start <= 0 でも len 分の窓で切る（PostgreSQL 互換）。
        assert_eq!(substr("hello", -1.0, Some(4.0)).unwrap(), "he");
        // 範囲外は空文字。
        assert_eq!(substr("hello", 10.0, Some(2.0)).unwrap(), "");
    }

    #[test]
    fn substr_negative_length_is_rejected() {
        let err = substr("hello", 1.0, Some(-1.0)).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn substr_is_char_indexed_for_multibyte_text() {
        assert_eq!(substr("あいうえお", 2.0, Some(2.0)).unwrap(), "いう");
    }

    /// codex-review P1 指摘の回帰テスト: `i64::MAX as f64` は丸めにより `2^63`
    /// になるため、境界比較を `> (i64::MAX as f64)` のままにすると
    /// `v == 2^63`（`9223372036854775808.0`）を上限超過として拒否できず、
    /// 後続の `as i64` が `i64::MAX` へ飽和して範囲外引数を誤って受理して
    /// しまっていた。`arg_as_i64` が正しい排他的境界（`2^63`／`-2^63`）で
    /// 拒否・受理することを固定する。
    #[test]
    fn arg_as_i64_rejects_exact_two_pow_63_and_accepts_i64_max_min() {
        // `2^63`（`i64::MAX` を `f64` へキャストした結果と一致する値）は
        // `i64` の表現域外のため拒否する。
        let err = arg_as_i64(9_223_372_036_854_775_808.0, "start").unwrap_err();
        assert_eq!(err.wire_code(), "22000");

        // `i64::MAX` 自体（`f64` では `2^63` へ丸められるが、丸め後の値でも
        // 排他的境界の直前として受理してよい。実際に表現可能な最大の `f64`
        // 整数値としては `2^63` になるため、ここでは境界直下の
        // `2^63 - 1024.0`〔`f64` で正確に表現できる `i64::MAX` 近傍の整数〕を
        // 使い、`i64::MAX` に飽和させず正しい値を返すことを確認する）。
        let near_max = 9_223_372_036_854_774_784.0_f64; // 2^63 - 1024
        assert_eq!(arg_as_i64(near_max, "start").unwrap(), near_max as i64);

        // `-2^63`（`i64::MIN`）は表現域の下限として受理する。
        assert_eq!(
            arg_as_i64(-9_223_372_036_854_775_808.0, "start").unwrap(),
            i64::MIN
        );

        // `-2^63` を下回る値（`-2^63 - ε` 相当。`f64` の精度上ここでは
        // 十分大きく下回る値で確認する）は拒否する。
        let err = arg_as_i64(-9_223_372_036_854_777_856.0, "start").unwrap_err();
        assert_eq!(err.wire_code(), "22000");

        // 非有限値・非整数値の既存契約も維持されていることを確認する。
        let err = arg_as_i64(f64::NAN, "start").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
        let err = arg_as_i64(f64::INFINITY, "start").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
        let err = arg_as_i64(f64::NEG_INFINITY, "start").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
        let err = arg_as_i64(1.5, "start").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    /// `SUBSTR` 経由でも境界値拒否が効くことを固定する（`arg_as_i64` の単体
    /// テストと合わせた end-to-end 確認）。
    #[test]
    fn substr_start_at_two_pow_63_is_rejected() {
        let err = substr("hello", 9_223_372_036_854_775_808.0, Some(1.0)).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn concat2_joins_without_separator() {
        assert_eq!(concat2("foo", "bar").unwrap(), "foobar");
    }

    #[test]
    fn trim_removes_only_half_width_space() {
        assert_eq!(trim("  hi  ").unwrap(), "hi");
        // タブは除去しない（PostgreSQL 既定）。
        assert_eq!(trim("\thi\t").unwrap(), "\thi\t");
    }

    #[test]
    fn replace_all_occurrences() {
        assert_eq!(replace("abcabc", "a", "X").unwrap(), "XbcXbc");
    }

    #[test]
    fn replace_with_empty_from_is_identity() {
        assert_eq!(replace("abc", "", "X").unwrap(), "abc");
    }

    /// レビュー指摘（PR 自己レビュー）: `REPLACE(s, from, to)` は `from` が短く
    /// `to` が長い場合に出現回数へ比例して乗算的に膨張しうる。
    /// `String::replace` を呼ぶ前に見積り計算で拒否できることを固定する
    /// （実際に `MAX_RESULT_LEN` を超える巨大な `String::replace` 確保を
    /// 発生させずに `54000` を返すことが本テストの主眼）。
    #[test]
    fn replace_rejects_before_allocating_when_expansion_exceeds_limit() {
        // 1 文字を 1 KiB 文字列へ 5000 回置換すると期待長は約 5 MiB となり、
        // `MAX_RESULT_LEN`（4 MiB）を超える。
        let s = "a".repeat(5000);
        let to = "x".repeat(1024);
        let err = replace(&s, "a", &to).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn position_is_one_indexed_and_char_based() {
        assert_eq!(position("hello", "llo"), 3.0);
        assert_eq!(position("hello", "z"), 0.0);
        assert_eq!(position("hello", ""), 1.0);
        assert_eq!(position("あいうえお", "うえ"), 3.0);
    }
}
