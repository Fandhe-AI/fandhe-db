//! `NUMERIC(p, s)` 列型の内部表現・パース・整形（TABLE-13〔検討中〕・TASK-197、
//! Issue #885）。十進固定小数を `unscaled × 10^-scale`（`i128` の符号付き整数
//! `unscaled` と `u8` の `scale`）で表現する自作実装。外部クレートに依存しない
//! （dependency-policy）。
//!
//! [`catalog::ColumnType::Numeric`]（列宣言）・[`row_codec`]（行バイト表現）・
//! `sql::parser`（INSERT/UPDATE/UPSERT リテラル束縛）から呼ばれる。列の
//! `scale` は行には持たずカタログの列型のみが正とする（TABLE-13 追記参照。
//! ADR: `docs/design/column-type-extension.md`「#885 追記」節）。

use std::fmt;

/// `NUMERIC` 列が宣言できる最大精度（全体桁数）。`i128::MAX` は 10^38 未満のため、
/// 38 桁までの `unscaled` は必ず `i128` へ収まる。
pub const MAX_PRECISION: u8 = 38;

/// 10 進固定小数リテラルの解析前の入力長上限（バイト）。DoS 防止のため解析前に
/// 検査する（coding-rust.md: untrusted 入力の長さ検証）。
pub const MAX_LITERAL_LEN: usize = 1024;

/// 10^0 〜 10^38 の参照表。`p`（列精度）は untrusted 経由（カタログ decode）にも
/// なり得るため、添字アクセスは必ず [`pow10`] 経由にする。
const POW10: [i128; 39] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
    10_000_000_000_000_000_000,
    100_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000_000_000_000,
    1_000_000_000_000_000_000_000_000_000_000_000_000,
    10_000_000_000_000_000_000_000_000_000_000_000_000,
    100_000_000_000_000_000_000_000_000_000_000_000_000,
];

// 10^38 − 1 は i128::MAX（約 1.7 × 10^38）未満に収まる。境界（`MAX_PRECISION`）を
// 超える添字で `POW10` を引くコードが将来入っても panic しないよう、参照は必ず
// `pow10()` 経由にする（このアサーションは配列サイズの自己整合性のみを保証する）。
const _: () = assert!(POW10.len() == (MAX_PRECISION as usize) + 1);

/// `POW10[p]` の安全な参照。範囲外（`p > 38`）は `None`（呼び出し元は列精度の
/// 検証を経由済みのはずだが、untrusted なカタログ decode からも呼ばれるため
/// 添字アクセスを使わず fail-closed にする）。
fn pow10(p: u8) -> Option<i128> {
    POW10.get(p as usize).copied()
}

/// `NUMERIC` 列の値パース・decode で発生し得るエラー。呼び出し元
/// （`sql::parser`・`row_codec`）が `wire_code`（`22000`/`22003`）へ写像する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumericError {
    /// 文法違反（空・符号のみ・複数の `.`・桁の無い指数・非数字・長さ超過等）。
    Malformed(String),
    /// 列の `NUMERIC(p, s)` に対して桁あふれ（丸め後の整数部が `p - s` 桁を
    /// 超える）。
    OutOfRange,
}

impl fmt::Display for NumericError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NumericError::Malformed(msg) => write!(f, "malformed numeric literal: {msg}"),
            NumericError::OutOfRange => write!(f, "numeric value out of range for column"),
        }
    }
}

impl std::error::Error for NumericError {}

/// `NUMERIC(p, s)` 列 1 個の値。`unscaled × 10^-scale` を表す（D2）。
///
/// 行バイト表現（[`crate::row_codec`]）は `scale` を持たずカタログの列型のみを
/// 正とするため、`Decimal` を独立に持ち回る場面（束縛・投影・RETURNING・wire
/// 出力）では常に列の `scale` と対にして扱う契約とする。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal {
    unscaled: i128,
    scale: u8,
}

impl Decimal {
    /// `unscaled`・`scale` から検証付きで構築する（呼び出し元は列の `scale`
    /// と一致していることを保証する契約だが、この契約は本コンストラクタが
    /// 公開 API（`row_codec`・wire-server・テストから到達）であるため型
    /// レベルでは強制できない）。`-0` は正規化（`unscaled == 0` のとき符号は
    /// 常に非負）する。
    ///
    /// `scale > MAX_PRECISION` は `NumericError::OutOfRange` を返す
    /// （fail-closed）。以前の実装は範囲外 `scale` を `MAX_PRECISION` へ
    /// 暗黙に丸めていたが、`from_parts(1, 39)` が本来表す `10^-39` の値を
    /// 検証なしに `10^-38` という別の数値へ変えてしまい、以後の `Display`・
    /// ハッシュ・格納判定のすべてが誤った値を正として扱う結果になっていた
    /// （P1 指摘対応）。呼び出し元が範囲外を渡し得る限り、丸めではなく
    /// 明示的な拒否で不変条件を守る。
    pub fn from_parts(unscaled: i128, scale: u8) -> Result<Self, NumericError> {
        if scale > MAX_PRECISION {
            return Err(NumericError::OutOfRange);
        }
        Ok(Decimal {
            unscaled: if unscaled == 0 { 0 } else { unscaled },
            scale,
        })
    }

    pub fn unscaled(&self) -> i128 {
        self.unscaled
    }

    pub fn scale(&self) -> u8 {
        self.scale
    }

    /// 列の精度 `p` に対して `|unscaled| < 10^p` を満たすか（TABLE-13 の
    /// decode 側検証・`row_codec::decode_row` から呼ばれる）。`p` が範囲外
    /// （`> MAX_PRECISION`）の場合は fail-closed に `false` を返す。
    pub fn fits_precision(&self, precision: u8) -> bool {
        match pow10(precision) {
            Some(limit) => self.unscaled.unsigned_abs() < limit.unsigned_abs(),
            None => false,
        }
    }
}

/// `scale` が異なりうる 2 つの `Decimal` を、丸めずに正確な数値として比較する
/// （TABLE-13・TASK-199、Issue #891。`declarative_filter::MetadataFilter::matches`
/// の `NUMERIC` 列比較述語から呼ばれる）。
///
/// 手順: まず整数部（`div_euclid`。負数でも余りが非負になる床除算）を比較し、
/// 異なればその大小関係を返す。整数部が一致する場合のみ小数部（`rem_euclid`。
/// 常に `[0, 10^scale)` の範囲）を大きい方の `scale` へ揃えて比較する。揃えた後の
/// 小数部は `10^MAX_PRECISION` 未満（`Decimal` の `scale` は構築時点で
/// `0..=MAX_PRECISION` に検証済み）に収まるため `i128` の乗算がオーバーフローしない。
/// クロス乗算（`a_frac * 10^b_scale` のように両者の全桁を掛け合わせる）ではなく
/// 差分桁数だけ底上げする方式のため、桁数の上限に対して安全である。
pub fn cmp_exact(a: &Decimal, b: &Decimal) -> std::cmp::Ordering {
    let a_div = pow10(a.scale).unwrap_or(1);
    let b_div = pow10(b.scale).unwrap_or(1);
    let a_int = a.unscaled.div_euclid(a_div);
    let b_int = b.unscaled.div_euclid(b_div);
    match a_int.cmp(&b_int) {
        std::cmp::Ordering::Equal => {}
        other => return other,
    }
    let a_frac = a.unscaled.rem_euclid(a_div);
    let b_frac = b.unscaled.rem_euclid(b_div);
    let max_scale = a.scale.max(b.scale);
    // `checked_pow`/`checked_mul` が使えない場面ではないが、`scale` は事前に
    // `MAX_PRECISION` 以下と検証済みのため差分桁の底上げは常に成功する契約。
    // それでも untrusted 経由の不変条件破れに備えて `unwrap_or` で fail-closed
    // （桁上げが失敗した側は 0 とみなし、少なくとも整数部一致の判定は保つ）。
    let a_scale_up = pow10(max_scale - a.scale).unwrap_or(1);
    let b_scale_up = pow10(max_scale - b.scale).unwrap_or(1);
    let a_frac_scaled = a_frac.saturating_mul(a_scale_up);
    let b_frac_scaled = b_frac.saturating_mul(b_scale_up);
    a_frac_scaled.cmp(&b_frac_scaled)
}

/// `literal`（`parse_literal_exact` が返す任意の `scale` を持つ `NUMERIC`
/// 比較リテラル）を、列固定の `col_scale` における `unscaled`（`i128`）の
/// 範囲境界へ正規化する（`sql::scalar_index::ScalarIndex::candidates_for` の
/// `NUMERIC` 列 `TypedCompare` 述語専用。TABLE-13・TASK-199、Issue #893）。
///
/// 列の値は常に `col_scale` ちょうどで格納される（`row_codec` が列型の
/// `scale` に固定してデコードする契約）ため、`unscaled` 同士の大小比較が
/// そのまま実数比較になる。`literal` 側は任意の `scale` を持ちうるため、
/// [`cmp_exact`] と同じ「丸めない」比較意味論を保ったまま `col_scale` 側の
/// 整数境界へ変換する。
///
/// `literal` を `col_scale` へ厳密に表現できない（`literal` の方が細かい
/// 桁を持つ）場合は、床（`div_euclid`/`rem_euclid`。[`cmp_exact`] と同じ
/// 負数でも余りが常に `[0, divisor)` になる方向）で位置を求め、余りの有無で
/// `Gt`/`Ge`/`Lt`/`Le`/`Eq` の境界を導出する（`sql::scalar_plan::id_bounds`
/// の非整数リテラル処理と同型のパターン）。桁合わせの乗除がオーバーフロー
/// する場合は `None`（呼び出し元は索引未対応として全走査へ縮退する契機に
/// する。fail-closed）。
pub(crate) fn rescale_bounds_for_column(
    literal: &Decimal,
    col_scale: u8,
    op: crate::declarative_filter::CompareOp,
) -> Option<(std::ops::Bound<i128>, std::ops::Bound<i128>)> {
    use crate::declarative_filter::CompareOp;
    use std::ops::Bound;

    // 呼び出し元が「一致 0 件」を明確に区別できるよう、キー値と無関係に
    // 常に空集合になる正準表現（`Included(0)..Excluded(0)` は常に
    // `start == end`。`sql::scalar_plan::id_bounds` の `EMPTY` と同じ手法）。
    const EMPTY: (std::ops::Bound<i128>, std::ops::Bound<i128>) =
        (Bound::Included(0), Bound::Excluded(0));

    let lit_scale = literal.scale();
    let lit_unscaled = literal.unscaled();

    if lit_scale <= col_scale {
        // 列の方が精度が高い（または同じ）: 桁上げは常に厳密（丸めなし）。
        let up = pow10(col_scale - lit_scale)?;
        let exact = lit_unscaled.checked_mul(up)?;
        return Some(match op {
            CompareOp::Eq => (Bound::Included(exact), Bound::Included(exact)),
            CompareOp::Gt => match exact.checked_add(1) {
                Some(l) => (Bound::Included(l), Bound::Unbounded),
                None => EMPTY,
            },
            CompareOp::Ge => (Bound::Included(exact), Bound::Unbounded),
            CompareOp::Lt => match exact.checked_sub(1) {
                Some(u) => (Bound::Unbounded, Bound::Included(u)),
                None => EMPTY,
            },
            CompareOp::Le => (Bound::Unbounded, Bound::Included(exact)),
        });
    }

    // 列の方が精度が低い: `literal` を `col_scale` の格子へ切り捨て、余りの
    // 有無で境界を調整する。
    let divisor = pow10(lit_scale - col_scale)?;
    let floor_int = lit_unscaled.div_euclid(divisor);
    let has_remainder = lit_unscaled.rem_euclid(divisor) != 0;

    Some(match op {
        CompareOp::Eq => {
            if has_remainder {
                EMPTY
            } else {
                (Bound::Included(floor_int), Bound::Included(floor_int))
            }
        }
        // `r > l`: `l` の小数部（`has_remainder`）の有無によらず、常に
        // `floor_int + 1` 以上（`l` が整数の場合は `floor_int` 自身も除外、
        // 小数を持つ場合も次の整数格子点から一致するため同じ式になる）。
        CompareOp::Gt => match floor_int.checked_add(1) {
            Some(l) => (Bound::Included(l), Bound::Unbounded),
            None => EMPTY,
        },
        CompareOp::Ge => {
            if has_remainder {
                match floor_int.checked_add(1) {
                    Some(l) => (Bound::Included(l), Bound::Unbounded),
                    None => EMPTY,
                }
            } else {
                (Bound::Included(floor_int), Bound::Unbounded)
            }
        }
        CompareOp::Lt => {
            if has_remainder {
                (Bound::Unbounded, Bound::Included(floor_int))
            } else {
                match floor_int.checked_sub(1) {
                    Some(u) => (Bound::Unbounded, Bound::Included(u)),
                    None => EMPTY,
                }
            }
        }
        // `r <= l`: `has_remainder` の有無によらず常に `floor_int` 以下
        // （`l` が整数のときはそのまま、小数を持つときも `r` は整数のため
        // `r <= floor_int + frac(0<frac<1)` は `r <= floor_int` と同値）。
        CompareOp::Le => (Bound::Unbounded, Bound::Included(floor_int)),
    })
}

impl fmt::Display for Decimal {
    /// 正規テキスト表現（D8）。符号・整数部・（`scale > 0` のときのみ）`.` と
    /// ちょうど `scale` 桁にゼロ埋めした小数部。先頭ゼロ・指数表記・trailing
    /// `-0` を持たない（wire の DataRow テキスト・HTTP JSON number 化の
    /// いずれからも共有される正規形）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = self.scale as u32;
        let divisor = match pow10(self.scale) {
            Some(d) => d,
            // scale はカタログ経由で 0..=38 に検証済みのはず。防御的に
            // 桁なし表示へ縮退する（表示は fail-closed 対象外の整形処理のため
            // panic を避けるだけで十分）。
            None => return write!(f, "{}", self.unscaled),
        };
        let sign = if self.unscaled < 0 { "-" } else { "" };
        let abs = self.unscaled.unsigned_abs();
        let int_part = abs / (divisor.unsigned_abs());
        if scale == 0 {
            return write!(f, "{sign}{int_part}");
        }
        let frac_part = abs % (divisor.unsigned_abs());
        write!(
            f,
            "{sign}{int_part}.{frac_part:0width$}",
            width = scale as usize
        )
    }
}

/// 数字列の 1 パス読み取り状態（整数部・小数部を通しで走査する）。
struct DigitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> DigitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        DigitReader { bytes, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn advance(&mut self) {
        // `checked_add` は使わず単純加算だが、`pos` は `bytes.len()`
        // （`MAX_LITERAL_LEN` 以下に検証済み）を超えて呼ばれないため
        // オーバーフローしない（`peek` が事前に `None` を返し `advance` を
        // 呼ばせない構造）。
        self.pos += 1;
    }
}

/// 指数部の絶対値の上限（クランプ値）。仮数部は最大 `MAX_LITERAL_LEN` 桁のため、
/// これを超える指数は結果（22003 か scale への丸めで 0）を変えない。指数の
/// 数字列は最大 1024 桁あり得るので、累積は必ずこの値で頭打ちにして `i64` の
/// 桁あふれを防ぐ。
const EXP_CLAMP: i64 = 4096;

/// [`scan_literal`] が返す走査結果（入力文字列を借用し、アロケーションしない）。
struct LiteralParts<'a> {
    negative: bool,
    /// 整数部の数字列（先頭ゼロを含む）。
    int_digits: &'a [u8],
    /// 小数部の数字列。
    frac_digits: &'a [u8],
    /// 指数（`[-EXP_CLAMP, EXP_CLAMP]` にクランプ済み）。指数部が無ければ 0。
    exp: i64,
}

/// リテラル文法 `[+-]?(digits)?(\.digits?)?([eE][+-]?digits)?` の唯一の走査実装
/// （TABLE-13・NOSQL-17、Issue #1358）。[`parse_for_column`] と
/// [`parse_literal_exact`] が共有し、文法の SSOT を 1 箇所に保つ。
/// 仮数部（整数部・小数部）に 1 桁以上の数字が必要。長さ・ASCII 検査を
/// 走査前に行う（untrusted 入力。coding-rust.md）。添字アクセスは使わない。
fn scan_literal(text: &str) -> Result<LiteralParts<'_>, NumericError> {
    if text.len() > MAX_LITERAL_LEN {
        return Err(NumericError::Malformed("literal too long".to_string()));
    }
    if !text.is_ascii() {
        return Err(NumericError::Malformed("literal must be ASCII".to_string()));
    }
    let bytes = text.as_bytes();
    let mut reader = DigitReader::new(bytes);

    let negative = match reader.peek() {
        Some(b'+') => {
            reader.advance();
            false
        }
        Some(b'-') => {
            reader.advance();
            true
        }
        _ => false,
    };

    let int_start = reader.pos;
    while reader.peek().is_some_and(|c| c.is_ascii_digit()) {
        reader.advance();
    }
    let int_end = reader.pos;

    let (mut frac_start, mut frac_end) = (int_end, int_end);
    if reader.peek() == Some(b'.') {
        reader.advance();
        frac_start = reader.pos;
        while reader.peek().is_some_and(|c| c.is_ascii_digit()) {
            reader.advance();
        }
        frac_end = reader.pos;
    }

    if int_end == int_start && frac_end == frac_start {
        return Err(NumericError::Malformed("literal has no digits".to_string()));
    }

    let mut exp: i64 = 0;
    if matches!(reader.peek(), Some(b'e' | b'E')) {
        reader.advance();
        let exp_negative = match reader.peek() {
            Some(b'+') => {
                reader.advance();
                false
            }
            Some(b'-') => {
                reader.advance();
                true
            }
            _ => false,
        };
        let mut saw_exp_digit = false;
        while let Some(c) = reader.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            saw_exp_digit = true;
            exp = exp
                .saturating_mul(10)
                .saturating_add(i64::from(c - b'0'))
                .min(EXP_CLAMP);
            reader.advance();
        }
        if !saw_exp_digit {
            return Err(NumericError::Malformed(
                "exponent has no digits".to_string(),
            ));
        }
        if exp_negative {
            exp = -exp;
        }
    }

    // 未消費のバイトが残っていれば文法違反（空白・複数の `.`・指数後の `.` 等）。
    if reader.peek().is_some() {
        return Err(NumericError::Malformed(
            "literal contains unsupported trailing characters".to_string(),
        ));
    }

    Ok(LiteralParts {
        negative,
        int_digits: bytes.get(int_start..int_end).unwrap_or_default(),
        frac_digits: bytes.get(frac_start..frac_end).unwrap_or_default(),
        exp,
    })
}

/// `NUMERIC(precision, scale)` 列向けにリテラル文字列を解析し、束縛値
/// `Decimal` を返す（D4・D5）。
///
/// 受理する文法: `[+-]?(digits)?(\.digits?)?([eE][+-]?digits)?`（整数部・
/// 小数部の少なくとも一方に 1 桁以上必要。指数部は Issue #1358 で受理）。
/// 空白・`NaN`/`Infinity`・複数の `.`・指数部の小数・桁の無い指数は拒否する。
/// 指数は小数点位置のシフトとして扱い、シフト後の値に対して丸め
/// （half away from zero、D4）と桁検査を行う。シフト後の整数部が
/// `precision - scale` 桁を超える場合（丸めの繰り上がり含む）は `OutOfRange`
/// （`22003`）。極端に大きな負の指数は 0 へ丸まり、非ゼロ仮数の極端に大きな
/// 正の指数は `OutOfRange` になる（指数は [`EXP_CLAMP`] で頭打ち）。
///
/// アロケーションなしで走査し、`unwrap`/`expect`/添字アクセスを使わない
/// （coding-rust.md）。
pub fn parse_for_column(text: &str, precision: u8, scale: u8) -> Result<Decimal, NumericError> {
    let parts = scan_literal(text)?;
    let int_len = parts.int_digits.len();
    let total_len = int_len + parts.frac_digits.len();
    // 仮想数字列 D = 整数部 ++ 小数部。k 番目の数字（範囲外・負は暗黙の 0）。
    let digit_at = |k: i64| -> i128 {
        let Ok(idx) = usize::try_from(k) else {
            return 0;
        };
        let byte = if idx < int_len {
            parts.int_digits.get(idx)
        } else {
            parts.frac_digits.get(idx - int_len)
        };
        byte.map_or(0, |b| i128::from(b.saturating_sub(b'0')))
    };
    // 小数点位置（D の先頭から数えた桁数）。負・len(D) 超過もあり得る。
    let point = (int_len as i64) + parts.exp;

    // 整数部: 有効桁（先頭の連続する `0` は数えない。`NUMERIC(38,38)` へ
    // `0.999...9` を通すために必要）が `precision - scale` を超えた時点で
    // `OutOfRange` にする（丸め前に整数部だけで超過が確定するケースの早期終了。
    // 丸めによる繰り上がりは最後に fits_precision で再検査する）。
    let int_limit = u32::from(precision.saturating_sub(scale));
    let mut int_value: i128 = 0;
    let mut int_sig: u32 = 0;
    let int_take = usize::try_from(point.max(0)).unwrap_or(0).min(total_len);
    for k in 0..int_take {
        let digit = digit_at(k as i64);
        if int_sig == 0 && digit == 0 {
            continue;
        }
        int_sig += 1;
        if int_sig > int_limit {
            return Err(NumericError::OutOfRange);
        }
        int_value = int_value
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit))
            .ok_or(NumericError::OutOfRange)?;
    }
    // D を超える位置までのシフト分は暗黙の末尾ゼロ。整数部が非ゼロのときだけ
    // 有効桁に数える。
    if point > total_len as i64 && int_sig > 0 {
        let extra = u32::try_from(point - total_len as i64).unwrap_or(u32::MAX);
        if int_sig.saturating_add(extra) > int_limit {
            return Err(NumericError::OutOfRange);
        }
        // `extra <= int_limit <= 38` が上の検査で保証される。
        let mul = pow10(extra as u8).ok_or(NumericError::OutOfRange)?;
        int_value = int_value.checked_mul(mul).ok_or(NumericError::OutOfRange)?;
    }

    // 小数部: 小数点以降 `scale` 桁（暗黙ゼロ込み）と、その次の桁（丸め判定）。
    let mut frac_value: i128 = 0;
    for j in 0..i64::from(scale) {
        frac_value = frac_value
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit_at(point.saturating_add(j))))
            .ok_or(NumericError::OutOfRange)?;
    }
    let round_up = digit_at(point.saturating_add(i64::from(scale))) >= 5;

    let scale_mul = pow10(scale).ok_or(NumericError::OutOfRange)?;
    let mut unscaled = int_value
        .checked_mul(scale_mul)
        .and_then(|v| v.checked_add(frac_value))
        .ok_or(NumericError::OutOfRange)?;
    if round_up {
        unscaled = unscaled.checked_add(1).ok_or(NumericError::OutOfRange)?;
    }
    if parts.negative && unscaled != 0 {
        unscaled = -unscaled;
    }

    // `scale` は呼び出し元（カタログ経由の列定義）で `0..=MAX_PRECISION` に
    // 検証済みのはずだが、`from_parts` の fail-closed 契約に合わせ `?` で
    // 伝播する（本関数はもともと `Result` を返すため追加コストはない）。
    let value = Decimal::from_parts(unscaled, scale)?;
    // 丸め後の繰り上がりで桁あふれし得るため、最終値で改めて検査する
    // （D4: `999.995` → 丸め後 `1000.00` は `22003`）。
    if !value.fits_precision(precision) {
        return Err(NumericError::OutOfRange);
    }
    Ok(value)
}

/// `NUMERIC` 列の範囲比較リテラル（TABLE-13・TASK-199、Issue #891・
/// `declarative_filter::DeclarativeFilter::compare`/`compare_numeric_literal`）
/// を、**列の `scale` に丸めず**リテラル自身の小数桁数をそのまま `scale` として
/// [`parse_for_column`] へ委譲する。範囲比較の意味論（`x > 1.005` が列の
/// scale で丸めた `1.01` 比較に化けない）を保つための第 2 の入口だが、
/// 実際のトークン走査・文法検証は [`parse_for_column`] 1 箇所を共有するため
/// 「第 2 のパーサー」にはならない。
///
/// 正確な `scale` は `max(0, 小数部の桁数 - 指数)`（指数表記は Issue #1358。
/// `1e-3` は scale 3、`1.5e3` は scale 0）。これが [`MAX_PRECISION`] を超える、
/// または整数部が 38 桁に収まらない場合は `NumericError::OutOfRange`
/// （範囲比較に使う `NUMERIC` 値としては桁数過多で不成立）。
pub fn parse_literal_exact(text: &str) -> Result<Decimal, NumericError> {
    let parts = scan_literal(text)?;
    let exact = (parts.frac_digits.len() as i64).saturating_sub(parts.exp);
    let scale = u8::try_from(exact.max(0)).map_err(|_| NumericError::OutOfRange)?;
    if scale > MAX_PRECISION {
        return Err(NumericError::OutOfRange);
    }
    parse_for_column(text, MAX_PRECISION, scale)
}

/// `AVG(NUMERIC(p,s))`（TABLE-13・SQL-13、Issue #892・D6）向けの長除算。
/// `sum`（`from_scale` 桁の unscaled 累積値）を `count`（非 NULL 行数、`> 0`）で
/// 割り、結果を `to_scale`（`>= from_scale`）桁の unscaled 整数として返す。
///
/// `sum * 10^(to_scale - from_scale)` を素直に計算すると `to_scale` が大きい
/// 場合に中間値が `i128` を超えて桁あふれし得るため、1 桁ずつ商を確定する
/// 長除算方式を取る（各ステップの剰余は常に `count` 未満 = `u64::MAX` 以下の
/// ため、`remainder * 10` は `i128`/`u128` の範囲内に収まり桁あふれしない）。
/// 最後の剰余で丸め方向を判定する（half away from zero。
/// [`parse_for_column`] の丸め規約と同じ）。
///
/// `count == 0` または `to_scale < from_scale` は呼び出し元
/// （`sql::aggregate::Accumulator::finish`）の不変条件違反であり、
/// [`NumericError::Malformed`] を返す（到達しない想定の防御）。
pub(crate) fn avg_unscaled(
    sum: i128,
    count: u64,
    from_scale: u8,
    to_scale: u8,
) -> Result<i128, NumericError> {
    if count == 0 {
        return Err(NumericError::Malformed(
            "AVG divisor must not be zero".to_string(),
        ));
    }
    if to_scale < from_scale {
        return Err(NumericError::Malformed(
            "AVG result scale must not be smaller than the input scale".to_string(),
        ));
    }

    let negative = sum < 0;
    let divisor: u128 = u128::from(count);
    let mut remainder: u128 = sum.unsigned_abs();
    let mut quotient: u128 = remainder / divisor;
    remainder %= divisor;

    let extra_scale = u32::from(to_scale - from_scale);
    for _ in 0..extra_scale {
        // `remainder < divisor <= u64::MAX` なので `remainder * 10` は
        // `u128` に必ず収まる（桁あふれしない）。
        remainder *= 10;
        let digit = remainder / divisor;
        remainder %= divisor;
        quotient = quotient
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit))
            .ok_or(NumericError::OutOfRange)?;
    }

    // 同じ理由（`remainder < divisor <= u64::MAX`）で `remainder * 2` も
    // 桁あふれしない。half away from zero: 剰余が除数の半分以上なら切り上げる。
    if remainder * 2 >= divisor {
        quotient = quotient.checked_add(1).ok_or(NumericError::OutOfRange)?;
    }

    let unscaled = i128::try_from(quotient).map_err(|_| NumericError::OutOfRange)?;
    Ok(if negative && unscaled != 0 {
        -unscaled
    } else {
        unscaled
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(unscaled: i128, scale: u8) -> Decimal {
        Decimal::from_parts(unscaled, scale).expect("test scale must be within MAX_PRECISION")
    }

    #[test]
    fn parses_basic_values() {
        assert_eq!(parse_for_column("1.50", 5, 2), Ok(d(150, 2)));
        assert_eq!(parse_for_column("-1.50", 5, 2), Ok(d(-150, 2)));
        assert_eq!(parse_for_column("+1.50", 5, 2), Ok(d(150, 2)));
        assert_eq!(parse_for_column("0", 5, 2), Ok(d(0, 2)));
        assert_eq!(parse_for_column("0.0", 5, 2), Ok(d(0, 2)));
    }

    #[test]
    fn accepts_leading_and_trailing_dot_forms() {
        assert_eq!(parse_for_column(".5", 5, 2), Ok(d(50, 2)));
        assert_eq!(parse_for_column("5.", 5, 2), Ok(d(500, 2)));
    }

    #[test]
    fn pads_short_fraction_with_zeros() {
        assert_eq!(parse_for_column("1.5", 5, 2), Ok(d(150, 2)));
        assert_eq!(parse_for_column("1", 5, 2), Ok(d(100, 2)));
    }

    #[test]
    fn rounds_half_away_from_zero() {
        assert_eq!(parse_for_column("1.005", 5, 2), Ok(d(101, 2)));
        assert_eq!(parse_for_column("-1.005", 5, 2), Ok(d(-101, 2)));
        assert_eq!(parse_for_column("1.004", 5, 2), Ok(d(100, 2)));
        assert_eq!(parse_for_column("1.999", 5, 2), Ok(d(200, 2)));
    }

    #[test]
    fn rounding_carry_can_overflow() {
        // NUMERIC(5,2) の最大は 999.99。丸め後 1000.00 になる入力は桁あふれ。
        assert_eq!(
            parse_for_column("999.995", 5, 2),
            Err(NumericError::OutOfRange)
        );
    }

    #[test]
    fn rejects_malformed_literals() {
        for bad in [
            "", "+", "-", "abc", "1.2.3", " 1", "1 ", "NaN", "Infinity", ".", "1e", "1e+", "1e-",
            "e3", ".e3", "+e3", "1e3.5", "1e3e4", "1e 3", "1 e3", "1ee3", "1e+-3",
        ] {
            assert!(
                matches!(
                    parse_for_column(bad, 10, 2),
                    Err(NumericError::Malformed(_))
                ),
                "expected Malformed for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_overlong_literal() {
        let too_long = "1".repeat(MAX_LITERAL_LEN + 1);
        assert_eq!(
            parse_for_column(&too_long, 38, 0),
            Err(NumericError::Malformed("literal too long".to_string()))
        );
    }

    #[test]
    fn boundary_precision_38_0() {
        let max = "9".repeat(38);
        let parsed = parse_for_column(&max, 38, 0).expect("max value must fit");
        assert_eq!(
            parsed.unscaled(),
            99_999_999_999_999_999_999_999_999_999_999_999_999i128
        );
        let neg = format!("-{max}");
        let parsed_neg = parse_for_column(&neg, 38, 0).expect("min value must fit");
        assert_eq!(
            parsed_neg.unscaled(),
            -99_999_999_999_999_999_999_999_999_999_999_999_999i128
        );

        let overflow = "1".to_string() + &"0".repeat(38);
        assert_eq!(
            parse_for_column(&overflow, 38, 0),
            Err(NumericError::OutOfRange)
        );
    }

    #[test]
    fn boundary_precision_38_38() {
        let frac = "9".repeat(38);
        let text = format!("0.{frac}");
        let parsed = parse_for_column(&text, 38, 38).expect("scale 38 boundary must fit");
        assert_eq!(parsed.scale(), 38);
    }

    #[test]
    fn boundary_precision_1_0() {
        assert_eq!(parse_for_column("9", 1, 0), Ok(d(9, 0)));
        assert_eq!(parse_for_column("10", 1, 0), Err(NumericError::OutOfRange));
        assert_eq!(parse_for_column("-10", 1, 0), Err(NumericError::OutOfRange));
    }

    #[test]
    fn negative_zero_normalizes() {
        let v = parse_for_column("-0.00", 5, 2).expect("negative zero literal must parse");
        assert_eq!(v.unscaled(), 0);
        assert_eq!(v.to_string(), "0.00");
    }

    #[test]
    fn display_formats_canonical_text() {
        assert_eq!(d(150, 2).to_string(), "1.50");
        assert_eq!(d(-150, 2).to_string(), "-1.50");
        assert_eq!(d(-50, 2).to_string(), "-0.50");
        assert_eq!(d(-1, 2).to_string(), "-0.01");
        assert_eq!(d(-100, 2).to_string(), "-1.00");
        assert_eq!(d(0, 2).to_string(), "0.00");
        assert_eq!(d(5, 0).to_string(), "5");
    }

    #[test]
    fn fits_precision_boundary_is_strict() {
        // NUMERIC(2,0) は -99..=99。100 は収まらない。
        assert!(d(99, 0).fits_precision(2));
        assert!(!d(100, 0).fits_precision(2));
        assert!(!d(-100, 0).fits_precision(2));
    }

    #[test]
    fn from_parts_rejects_out_of_range_scale() {
        // 呼び出し元がカタログ検証（`0..=MAX_PRECISION`）を経由せず
        // `MAX_PRECISION` 超過の `scale` を渡した場合、以前の実装は
        // `MAX_PRECISION` へ暗黙に丸めていたが、これは `from_parts(1, 39)`
        // が本来表す `10^-39` を検証なしに別の数値（`10^-38`）へ変えてしまう
        // 問題があった（P1 指摘対応）。丸めず明示的に拒否することを固定する。
        assert_eq!(Decimal::from_parts(1, 39), Err(NumericError::OutOfRange));
        assert_eq!(
            Decimal::from_parts(5, u8::MAX),
            Err(NumericError::OutOfRange)
        );
        // 境界値（`MAX_PRECISION` ちょうど）は受理される。
        assert!(Decimal::from_parts(1, MAX_PRECISION).is_ok());
    }

    #[test]
    fn cmp_exact_same_scale() {
        use std::cmp::Ordering;
        assert_eq!(cmp_exact(&d(100, 2), &d(100, 2)), Ordering::Equal);
        assert_eq!(cmp_exact(&d(100, 2), &d(101, 2)), Ordering::Less);
        assert_eq!(cmp_exact(&d(101, 2), &d(100, 2)), Ordering::Greater);
    }

    #[test]
    fn cmp_exact_different_scale_does_not_round() {
        use std::cmp::Ordering;
        // 1.005 (scale 3) と 1.01 (scale 2) は丸めれば等しく見えうるが、
        // 正確な数値としては 1.005 < 1.01 でなければならない（設計上の要件。
        // 列の scale で丸めて解析すると `x > 1.005` の意味が変わってしまう）。
        assert_eq!(cmp_exact(&d(1005, 3), &d(101, 2)), Ordering::Less);
        assert_eq!(cmp_exact(&d(101, 2), &d(1005, 3)), Ordering::Greater);
        // 1.10 (scale 2) と 1.100 (scale 3) は数値として等しい。
        assert_eq!(cmp_exact(&d(110, 2), &d(1100, 3)), Ordering::Equal);
    }

    #[test]
    fn cmp_exact_handles_sign_and_zero_boundary() {
        use std::cmp::Ordering;
        assert_eq!(cmp_exact(&d(-150, 2), &d(150, 2)), Ordering::Less);
        assert_eq!(cmp_exact(&d(-150, 2), &d(-100, 2)), Ordering::Less);
        assert_eq!(cmp_exact(&d(0, 2), &d(0, 5)), Ordering::Equal);
        // -0.5 (scale 1) と -0.50 (scale 2) はいずれも同じ値。
        assert_eq!(cmp_exact(&d(-5, 1), &d(-50, 2)), Ordering::Equal);
        // -0.01 と 0 の境界（負の小数部の床除算が正しく処理されること）。
        assert_eq!(cmp_exact(&d(-1, 2), &d(0, 2)), Ordering::Less);
    }

    #[test]
    fn cmp_exact_max_precision_does_not_overflow() {
        use std::cmp::Ordering;
        // 38 桁の最大値付近同士の比較で `i128` オーバーフローが起きないこと。
        let max_unscaled: i128 = POW10[MAX_PRECISION as usize] - 1;
        let a = d(max_unscaled, MAX_PRECISION);
        let b = d(max_unscaled, 0);
        // a は 0.999...9（38 桁）、b は 999...9（38 桁の整数）で a < b。
        assert_eq!(cmp_exact(&a, &b), Ordering::Less);
        assert_eq!(cmp_exact(&a, &a), Ordering::Equal);
    }

    // --- avg_unscaled（D6・Issue #892） -------------------------------------

    #[test]
    fn avg_unscaled_exact_division_needs_no_rounding() {
        // 1.00 + 2.00 = 3.00（sum=300, scale=2）を 2 件で割ると 1.50。
        assert_eq!(avg_unscaled(300, 2, 2, 4), Ok(15000));
    }

    #[test]
    fn avg_unscaled_rounds_half_away_from_zero() {
        // 0.01 / 3 = 0.003333... を scale 4 まで求めると 0.0033（切り捨て側）。
        assert_eq!(avg_unscaled(1, 3, 2, 4), Ok(33));
        // 0.05 / 2 = 0.025 を scale 2 で丸めると 0.03（4 捨 5 入ではなく
        // 5 は必ず切り上げる half away from zero）。
        assert_eq!(avg_unscaled(5, 2, 2, 2), Ok(3));
    }

    #[test]
    fn avg_unscaled_rounds_negative_values_away_from_zero() {
        // -0.05 / 2 = -0.025 → 絶対値が大きい方（-0.03）へ丸める。
        assert_eq!(avg_unscaled(-5, 2, 2, 2), Ok(-3));
    }

    #[test]
    fn avg_unscaled_same_scale_is_plain_division_with_rounding() {
        assert_eq!(avg_unscaled(10, 4, 0, 0), Ok(3));
        assert_eq!(avg_unscaled(10, 3, 0, 0), Ok(3));
    }

    #[test]
    fn avg_unscaled_rejects_zero_count() {
        assert!(matches!(
            avg_unscaled(1, 0, 2, 4),
            Err(NumericError::Malformed(_))
        ));
    }

    #[test]
    fn avg_unscaled_rejects_shrinking_scale() {
        assert!(matches!(
            avg_unscaled(1, 1, 4, 2),
            Err(NumericError::Malformed(_))
        ));
    }

    #[test]
    fn avg_unscaled_large_sum_does_not_overflow_intermediate_math() {
        // sum が大きく `sum * 10^(to_scale-from_scale)` を素直に計算すると
        // i128 を超える組み合わせでも、1 桁ずつ商を確定する長除算（各ステップの
        // 剰余は常に `count` 未満）のため、最終結果が 38 桁以内に収まる限り
        // オーバーフローしない（D6 のコメント参照）。
        let large_sum = 12_345_678_901_234_567_890i128; // 20 桁
        assert!(avg_unscaled(large_sum, 7, 0, 16).is_ok());
    }

    #[test]
    fn avg_unscaled_overflow_is_rejected() {
        // 除算結果が 38 桁を超える場合、桁を積み上げる過程で `checked_mul`/
        // `checked_add` が失敗し `OutOfRange` になる。
        let near_max = 99_999_999_999_999_999_999_999_999_999_999_999_999i128;
        assert_eq!(
            avg_unscaled(near_max, 1, 0, 30),
            Err(NumericError::OutOfRange)
        );
    }

    // ------------------------------------------------------------------
    // Issue #893: `rescale_bounds_for_column`（`NUMERIC` 列の二次索引境界）
    // ------------------------------------------------------------------

    fn bound_contains_i128(
        bounds: (std::ops::Bound<i128>, std::ops::Bound<i128>),
        v: i128,
    ) -> bool {
        use std::ops::Bound;
        let lower_ok = match bounds.0 {
            Bound::Included(l) => v >= l,
            Bound::Excluded(l) => v > l,
            Bound::Unbounded => true,
        };
        let upper_ok = match bounds.1 {
            Bound::Included(u) => v <= u,
            Bound::Excluded(u) => v < u,
            Bound::Unbounded => true,
        };
        lower_ok && upper_ok
    }

    /// [`rescale_bounds_for_column`] が返す境界が、列側の候補 `unscaled` 値
    /// （`col_scale` 固定）を [`cmp_exact`] による brute-force オラクルと
    /// 完全一致でふるい分けることを、列スケール・リテラルスケールの
    /// 大小関係の両方（リテラルの方が精度が高い／低い）・全比較演算子を
    /// 網羅して固定する。
    #[test]
    fn rescale_bounds_for_column_matches_cmp_exact_oracle() {
        use crate::declarative_filter::CompareOp;

        let col_scale = 2u8;
        let candidates: Vec<i128> = (-1000..=1000).collect();
        // リテラルの方が精度が低い（`lit_scale <= col_scale`）ケースと、
        // 精度が高い（`lit_scale > col_scale`。列の格子に乗らない値を含む）
        // ケースの双方を混在させる。
        let literals: Vec<Decimal> = vec![
            d(0, 0),
            d(5, 0),
            d(-5, 0),
            d(150, 2),
            d(-150, 2),
            d(1505, 3),  // 1.505: col_scale=2 の格子に乗らない
            d(-1505, 3), // -1.505: 同上
            d(1500, 3),  // 1.500: 格子に乗る（余り 0）
        ];
        let ops = [
            CompareOp::Eq,
            CompareOp::Gt,
            CompareOp::Ge,
            CompareOp::Lt,
            CompareOp::Le,
        ];
        for literal in &literals {
            for op in ops {
                let bounds = rescale_bounds_for_column(literal, col_scale, op)
                    .expect("finite literal within precision must not overflow");
                for &v in &candidates {
                    let col_value = d(v, col_scale);
                    let expected = op.accepts(cmp_exact(&col_value, literal));
                    let actual = bound_contains_i128(bounds, v);
                    assert_eq!(
                        actual, expected,
                        "op={op:?} literal={literal:?} col_scale={col_scale} v={v}"
                    );
                }
            }
        }
    }

    #[test]
    fn rescale_bounds_for_column_overflow_is_fail_closed() {
        use crate::declarative_filter::CompareOp;
        // 列側の精度が非常に高く、リテラルの桁上げが i128 を超える場合は
        // `None`（呼び出し元は索引未対応として全走査へ縮退する）。
        let huge_literal = d(99_999_999_999_999_999_999_999_999_999_999i128, 0);
        assert_eq!(
            rescale_bounds_for_column(&huge_literal, MAX_PRECISION, CompareOp::Eq),
            None
        );
    }

    fn col(text: &str, p: u8, sc: u8) -> Result<Decimal, NumericError> {
        parse_for_column(text, p, sc)
    }

    fn ok(text: &str, p: u8, sc: u8) -> Decimal {
        col(text, p, sc).unwrap_or_else(|e| panic!("{text:?} should parse: {e:?}"))
    }

    #[test]
    fn accepts_exponent_notation() {
        // Issue #1358: 指数は小数点位置のシフト。
        assert_eq!(ok("1e2", 5, 2), d(10000, 2));
        assert_eq!(ok("1E2", 5, 2), d(10000, 2));
        assert_eq!(ok("1e+2", 5, 2), d(10000, 2));
        assert_eq!(ok("1.5e2", 5, 2), d(15000, 2));
        assert_eq!(ok("1.5E+2", 5, 2), d(15000, 2));
        assert_eq!(ok("-1.5e2", 5, 2), d(-15000, 2));
        assert_eq!(ok(".5e1", 5, 2), d(500, 2));
        assert_eq!(ok("5.e1", 5, 2), d(5000, 2));
        assert_eq!(ok("1e0003", 10, 0), d(1000, 0));
        assert_eq!(ok("1e-1", 5, 2), d(10, 2));
    }

    #[test]
    fn exponent_shift_rounds_after_shift() {
        assert_eq!(ok("123456e-4", 5, 2), d(1235, 2));
        assert_eq!(ok("5e-3", 5, 2), d(1, 2));
        assert_eq!(ok("4e-3", 5, 2), d(0, 2));
        assert_eq!(ok("-5e-3", 5, 2), d(-1, 2));
        assert_eq!(ok("-4e-3", 5, 2), d(0, 2));
        assert_eq!(ok("0e99999", 5, 2), d(0, 2));
        assert_eq!(ok("-0e5", 5, 2), d(0, 2));
        assert_eq!(ok("1e-1000", 5, 2), d(0, 2));
    }

    #[test]
    fn trailing_garbage_with_int_overflow_is_malformed() {
        // 走査を先に行うため、文法違反は桁あふれより優先して 22000 になる
        // （どちらも fail-closed の拒否。Issue #1358 のレビュー指摘で固定）。
        assert!(matches!(
            col("99999999x", 5, 2),
            Err(NumericError::Malformed(_))
        ));
    }

    #[test]
    fn exponent_overflow_is_out_of_range() {
        assert_eq!(col("1e3", 5, 2), Err(NumericError::OutOfRange));
        assert_eq!(col("1.23456e4", 5, 2), Err(NumericError::OutOfRange));
        // 丸めの繰り上がりで 1000.00 になる。
        assert_eq!(col("9.99995e2", 5, 2), Err(NumericError::OutOfRange));
        assert_eq!(col("1e99999", 38, 0), Err(NumericError::OutOfRange));
        assert_eq!(col("1e38", 38, 0), Err(NumericError::OutOfRange));
    }

    #[test]
    fn exponent_boundaries() {
        let nines = "9".repeat(38);
        assert_eq!(
            ok("9.9999999999999999999999999999999999999e37", 38, 0),
            ok(&nines, 38, 0)
        );
        assert_eq!(ok("9.99e-1", 38, 38).scale(), 38);
        assert_eq!(ok("1e-38", 38, 38), d(1, 38));
    }

    #[test]
    fn huge_exponent_is_clamped_without_panic() {
        let long_exp = format!("1e{}", "9".repeat(1000));
        assert_eq!(col(&long_exp, 38, 0), Err(NumericError::OutOfRange));
        let long_neg = format!("1e-{}", "9".repeat(1000));
        assert_eq!(ok(&long_neg, 5, 2), d(0, 2));
        let long_zero_exp = format!("0e{}", "9".repeat(1000));
        assert_eq!(ok(&long_zero_exp, 5, 2), d(0, 2));
        let lead_zeros = format!("1e{}3", "0".repeat(1000));
        assert_eq!(ok(&lead_zeros, 10, 0), d(1000, 0));
    }

    #[test]
    fn parse_literal_exact_with_exponent() {
        assert_eq!(parse_literal_exact("1e-3"), Ok(d(1, 3)));
        assert_eq!(parse_literal_exact("1.5e3"), Ok(d(1500, 0)));
        assert_eq!(parse_literal_exact("1.50e1"), Ok(d(150, 1)));
        assert_eq!(parse_literal_exact("1e-50"), Err(NumericError::OutOfRange));
        assert_eq!(parse_literal_exact("1e40"), Err(NumericError::OutOfRange));
        assert_eq!(parse_literal_exact("1.005"), Ok(d(1005, 3)));
        assert_eq!(parse_literal_exact("7"), Ok(d(7, 0)));
        assert!(matches!(
            parse_literal_exact("1e"),
            Err(NumericError::Malformed(_))
        ));
    }
}
