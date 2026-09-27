//! 最小 JSON パーサ（依存追加なし・共有モジュール）。
//!
//! 元は `query_planner.rs` が Ollama `/api/generate` 応答パース専用として自作していた
//! パーサを、Issue #731 で共有モジュールへ移設したもの（挙動不変）。TASK-172・
//! NOSQL-8（spec ポインタ。`docs/spec/05-tasks.md` TASK-172）の NoSQL 表層が別途 JSON
//! パースを必要とする見込みのため、`query_planner` 固有のコンシューマに依存しない
//! 汎用パーサとして切り出している。
//!
//! 責務境界: untrusted なテキストを [`JsonValue`] へパースするところまでが責務で、
//! 意味的な検証（フィールドの型・件数・長さの上限等）は呼び出し元が行う
//! （`query_planner.rs::parse_expansion` が検索語件数・長さの意味的上限を独立に
//! 検証する例を参照）。エラーは常に単一の不透明な [`JsonError`] のみを返し、
//! 失敗理由（バイト位置等）を外部に漏らさない（fail-closed。security.md 準拠）。
//!
//! Issue #732（TASK-172・NOSQL-8 ポインタ）で、オブジェクトの重複キー拒否・数値
//! リテラルの RFC 8259 準拠の厳格化を行った（先頭ゼロ・小数部/指数部の数字欠落等
//! を拒否）。[`JsonError`] は [`crate::error_format::ClassifiedError`] を実装し、
//! `wire_code()` で `42601`（`UnsupportedSqlSyntax`）を返す。

use std::collections::BTreeMap;

use crate::error_format::{ClassifiedError, ErrorClass};

/// [`parse_json`] が受理するネスト深さの上限（スタック消費・DoS 対策）。
/// コンテナ（配列・オブジェクト）のネスト数の上限であり、`JsonParser::parse_object`・
/// `JsonParser::parse_array`（いずれも非公開）の入口で `depth >= MAX_JSON_DEPTH` を
/// 検査することで、空コンテナ（`[]`・`{}`）を最内に置いた場合でも 1 段多く受理して
/// しまう off-by-one（Issue #733 の境界テストで検出）が生じない形に統一している。
pub const MAX_JSON_DEPTH: usize = 16;
/// JSON 文字列リテラル 1 つあたりの最大文字数（トランスポート層の DoS 対策専用の
/// 緩い上限）。呼び出し元が意味的な上限（件数・長さ等）を独立に検証する前提の、
/// メモリ確保量を頭打ちさせるためだけの粗いバックストップ（狭すぎると正常な入力を
/// transport 層で拒否してしまうため、実用上の応答本文サイズに合わせた値とする）。
pub const MAX_JSON_STRING_CHARS: usize = 1024 * 1024;
/// JSON 配列・オブジェクトが保持できる要素数の上限（同上の理由でトランスポート層の
/// 粗い上限）。
pub const MAX_JSON_CONTAINER_ITEMS: usize = 65_536;

/// [`parse_json`] の失敗を表す不透明なマーカー型。詳細な失敗理由（どのバイト位置で
/// 何が起きたか）はあえて持たない。呼び出し元は自身のエラー型へ変換して扱う
/// （`query_planner.rs` の `impl From<JsonError> for PlanError` を参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError;

impl JsonError {
    /// SQLSTATE 風 `wire_code`（coding-rust.md「エラー型は SQLSTATE 風 wire_code の
    /// 設計に従う」）。[`ClassifiedError`] へ委譲する（実装型ごとに再定義しない。
    /// `tenant.rs::TenantWriteError::wire_code` と同じパターン）。
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }
}

/// [`parse_json`] の失敗はすべて「受理範囲外の構文」（`42601`）として写像する。
/// 重複キー・非 RFC 8259 数値・深さ/件数/長さ上限超過のいずれも同じ分類とし、
/// 内部詳細（バイト位置等）はクライアントへ運ばない（security.md P0）。
impl ClassifiedError for JsonError {
    fn error_class(&self) -> ErrorClass {
        ErrorClass::UnsupportedSqlSyntax
    }

    fn client_message(&self) -> String {
        "invalid JSON".to_string()
    }
}

/// [`parse_json`] が返す JSON 値。オブジェクトは `BTreeMap` で保持するため、
/// キーの反復順序はキー文字列の昇順で決定的になる（呼び出し元が結果を再現可能に
/// 扱えるようにするための設計選択。ハッシュ順や挿入順には依存しない）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(JsonNumber),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

/// JSON 数値リテラルの分類済み表現（Issue #823・PR #823 レビュー指摘対応）。
///
/// `f64` 単一表現では 2^53 を超える整数が桁落ちし、`insert.rs` の `id`
/// 疑似列のような「精度を落とさず整数として受理するか判定したい」呼び出し元が
/// 丸め後の値でしか判定できなかった（例: `9007199254740993` が最近傍の
/// 偶数である `9007199254740992` へ丸められ、上限検査を通過して別の行 id へ
/// 書き込まれ得た）。構文上「小数点・指数部を含まない整数リテラル」かどうかは
/// パース時点のテキストでのみ判定できるため、[`JsonParser::parse_number`] が
/// 精度を失う `f64` 変換の**前**に整数／小数を分類し、整数は `u64`／`i64` の
/// 無損失表現で保持する（オーバーフロー時のみ `Float` へ縮退。RFC 8259 上は
/// 有効な数値のため拒否はしない）。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonNumber {
    /// 小数点・指数部を含まない非負整数リテラル（`"-"` 始まりでない）。
    /// `u64` の範囲で無損失に表現できる。
    PosInt(u64),
    /// 小数点・指数部を含まない負の整数リテラル（`"-"` 始まり。`-0` を含む）。
    /// `i64` の範囲で無損失に表現できる。
    NegInt(i64),
    /// 小数点・指数部を含む、または整数として `u64`／`i64` に収まらない
    /// リテラル。`f64` 変換（丸めを伴いうる）で保持する。
    ///
    /// `text`（パース時点の生リテラル文字列）を併せて保持する（Issue #771
    /// レビュー指摘対応）。`value` だけを経由して `f32` を得ると
    /// `str -> f64 -> f32` の 2 回丸めになり、SQL 表層 `sql::parser::
    /// parse_vector_literal` の `str -> f32` 単一丸めと異なる結果になりうる
    /// （例: `"1.0000000596046448"` は `f64` 丸めで厳密に表現可能な値へ
    /// 丸まった後さらに `f32` へ丸めると `1.0` になるが、直接 `f32` へ
    /// 丸めると異なるビットパターンになる）。同一リテラル・同一
    /// `operation_id` を NoSQL・SQL 表層を跨いで再送した際に、ベクトル列の
    /// `content_hash`（`recovery::content_hash::push_vector`）が表層ごとに
    /// 異なり「同一内容の再送」（`23505`）ではなく「内容不一致」（`22023`）
    /// と誤判定されるのを防ぐため、[`JsonNumber::as_f32`] は `text` を
    /// [`parse_f32_text`] で直接 `f32` へ変換し `value` を経由しない。
    Float { value: f64, text: Box<str> },
}

impl JsonNumber {
    /// 全 variant を `f64` へ変換する（丸めを伴いうる。表示・比較等、精度が
    /// 問題にならない用途向け）。`NegInt(0)`（JSON の `-0`）は負のゼロへ
    /// 復元する: SQL 表層の `parse_vector_literal` は `-0` を `-0.0` として保持し
    /// `content_hash::push_vector` は符号ビットを含めてハッシュするため、
    /// 正のゼロへ潰すと表層横断の `operation_id` 再送判定（同一内容 `23505`／
    /// 内容不一致 `22023`）が食い違う（PR #823 codex-review 指摘）。
    pub fn as_f64(&self) -> f64 {
        match *self {
            JsonNumber::PosInt(n) => n as f64,
            JsonNumber::NegInt(0) => -0.0,
            JsonNumber::NegInt(n) => n as f64,
            JsonNumber::Float { value, .. } => value,
        }
    }

    /// `PosInt` のときのみ無損失な `u64` を返す（`id` 疑似列のように
    /// 非負整数を精度を落とさず要求する呼び出し元向け）。
    pub fn as_exact_u64(&self) -> Option<u64> {
        match *self {
            JsonNumber::PosInt(n) => Some(n),
            _ => None,
        }
    }

    /// 全 variant を `f32` へ変換する（Issue #771 レビュー指摘対応。SQL 表層
    /// `sql::parser::parse_vector_literal` の各要素と同一の丸め結果を保証する
    /// ための唯一の実装）。整数 variant は `f64` を経由せず直接キャストする
    /// （整数から `f32` への丸めは単一丸めであり、10 進整数テキストを直接
    /// `f32` へパースする場合と同じ最近接丸め結果になる。`NegInt(0)` は
    /// `as_f64` と同じ理由で `-0.0` へ復元する）。`Float` variant は保持した
    /// `text` を [`parse_f32_text`] で直接 `f32` へ変換し、`value`（`f64`）を
    /// 経由する 2 回丸めを避ける。非有限（`NaN`／`Infinity`。指数部が大きい
    /// リテラルが `f32` の範囲を超える場合を含む）または構文上パース不能な
    /// 場合は `None`（呼び出し元が fail-closed に拒否する）。
    pub fn as_f32(&self) -> Option<f32> {
        let v = match self {
            JsonNumber::PosInt(n) => *n as f32,
            JsonNumber::NegInt(0) => -0.0,
            JsonNumber::NegInt(n) => *n as f32,
            JsonNumber::Float { text, .. } => parse_f32_text(text).ok()?,
        };
        if v.is_finite() {
            Some(v)
        } else {
            None
        }
    }
}

/// `text` を `f32` として解析する（`str::parse::<f32>` の単一丸めのみを行い、
/// 非有限判定は呼び出し元に委ねる）。[`JsonNumber::as_f32`]・
/// `sql::parser::parse_vector_literal`（SQL ベクトルリテラルの各要素）の
/// 双方がこの関数を経由することで、JSON 数値リテラルと SQL 数値リテラルの
/// テキストが同一であれば変換結果もビット単位で一致することを保証する
/// （表層横断の `content_hash` 一貫性のための唯一の変換経路。第 2 の実装を
/// 作らない）。
pub fn parse_f32_text(text: &str) -> Result<f32, std::num::ParseFloatError> {
    text.parse::<f32>()
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), JsonError> {
        match self.bump() {
            Some(b) if b == expected => Ok(()),
            _ => Err(JsonError),
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        if depth > MAX_JSON_DEPTH {
            return Err(JsonError);
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            _ => Err(JsonError),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        // コンテナ入口でも深さ上限を検査する（空オブジェクトを最内に置いた入力が
        // `parse_value` の値ベース判定だけでは 1 段多く受理されてしまう off-by-one
        // を防ぐ。`parse_value` 側の `depth > MAX_JSON_DEPTH` は多層防御として残置）。
        if depth >= MAX_JSON_DEPTH {
            return Err(JsonError);
        }
        self.expect_byte(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            if map.len() >= MAX_JSON_CONTAINER_ITEMS {
                return Err(JsonError);
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect_byte(b':')?;
            let value = self.parse_value(depth + 1)?;
            // RFC 8259 は重複キーの扱いをパーサ依存とするが、後勝ちで無警告に
            // 上書きすると呼び出し元（NoSQL 表層等）が別実装と解釈を違えうる
            // （confusion attack の温床）。fail-closed に拒否する（Issue #732）。
            match map.entry(key) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(value);
                }
                std::collections::btree_map::Entry::Occupied(_) => return Err(JsonError),
            }
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(JsonError),
            }
        }
        Ok(JsonValue::Object(map))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        // `parse_object` と同じ理由でコンテナ入口でも深さ上限を検査する。
        if depth >= MAX_JSON_DEPTH {
            return Err(JsonError);
        }
        self.expect_byte(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            if items.len() >= MAX_JSON_CONTAINER_ITEMS {
                return Err(JsonError);
            }
            let value = self.parse_value(depth + 1)?;
            items.push(value);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(JsonError),
            }
        }
        Ok(JsonValue::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.expect_byte(b'"')?;
        let mut out = String::new();
        // 呼び出しのたびに `out.chars().count()` で文字列全体を再走査すると
        // エスケープを多く含む入力で O(n^2) になる（未認証入力が到達し得る
        // 経路で DoS になり得るため P0。codex-review PR #813 指摘）。
        // 1 文字（またはバイト列片）を追加するたびに増分カウントし、
        // 全体再走査を行わない O(n) 方式へ変更する。
        let mut char_count: usize = 0;
        loop {
            let b = self.bump().ok_or(JsonError)?;
            match b {
                b'"' => break,
                b'\\' => {
                    let esc = self.bump().ok_or(JsonError)?;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.parse_hex4()?;
                            if (0xd800..=0xdbff).contains(&cp) {
                                // 高位サロゲート: 直後に `\uXXXX` 形式の低位サロゲート
                                // が続く場合のみ、正規のサロゲートペアとして 1 個の
                                // 補助平面コードポイントへ復号する（絵文字等）。
                                if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                                    return Err(JsonError);
                                }
                                let low = self.parse_hex4()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    // 低位サロゲートが続かない = 孤立した高位サロゲート。
                                    // 破損文字列を U+FFFD へ丸めて正常応答として返すと
                                    // fail-closed 方針に反するため拒否する
                                    // （codex-review PR #252 P2 指摘）。
                                    return Err(JsonError);
                                }
                                let scalar = 0x10000u32
                                    + (u32::from(cp) - 0xd800) * 0x400
                                    + (u32::from(low) - 0xdc00);
                                out.push(char::from_u32(scalar).ok_or(JsonError)?);
                            } else if (0xdc00..=0xdfff).contains(&cp) {
                                // ペアの相方を伴わない孤立した低位サロゲートも不正な
                                // JSON 文字列表現であり、fail-closed に拒否する。
                                return Err(JsonError);
                            } else {
                                out.push(char::from_u32(u32::from(cp)).ok_or(JsonError)?);
                            }
                        }
                        _ => return Err(JsonError),
                    }
                }
                // 生の制御文字は JSON 仕様上不正（要エスケープ）。fail-closed に拒否する。
                0x00..=0x1f => return Err(JsonError),
                _ => {
                    // マルチバイト UTF-8 継続バイトも含め、そのままバイト列として
                    // 再構成する（`str::from_utf8` 相当の妥当性は元の `&str` 入力が
                    // 既に保証しているため、1 バイトずつ ASCII 相当のみを個別処理し
                    // それ以外はバイト列を後段でまとめて UTF-8 復元する）。
                    let start = self.pos - 1;
                    let mut end = self.pos;
                    while let Some(next) = self.peek() {
                        if next == b'"' || next == b'\\' || next < 0x20 {
                            break;
                        }
                        end += 1;
                        self.pos += 1;
                    }
                    // untrusted 入力経路のため添字アクセスではなく `get()` で明示的に
                    // 検証する（coding-rust.md）。`start`・`end` は上の走査で
                    // 常に `self.bytes` の範囲内に収まるが、範囲外を返す実装変更に
                    // 対しても fail-closed に振る舞う。
                    let Some(slice) = self.bytes.get(start..end) else {
                        return Err(JsonError);
                    };
                    let Ok(s) = std::str::from_utf8(slice) else {
                        return Err(JsonError);
                    };
                    // この 1 片ぶんの文字数だけを数える（`out` 全体は再走査しない）。
                    char_count += s.chars().count();
                    out.push_str(s);
                    if char_count > MAX_JSON_STRING_CHARS {
                        return Err(JsonError);
                    }
                    continue;
                }
            }
            // エスケープ経路はいずれも 1 文字だけを `out` へ追加する。
            char_count += 1;
            if char_count > MAX_JSON_STRING_CHARS {
                return Err(JsonError);
            }
        }
        Ok(out)
    }

    fn parse_hex4(&mut self) -> Result<u16, JsonError> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let b = self.bump().ok_or(JsonError)?;
            let digit = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(JsonError),
            };
            value = value
                .checked_mul(16)
                .and_then(|v| v.checked_add(u16::from(digit)))
                .ok_or(JsonError)?;
        }
        Ok(value)
    }

    fn parse_bool(&mut self) -> Result<JsonValue, JsonError> {
        // untrusted 入力経路のため添字アクセスではなく `get()` で明示的に検証する
        // （coding-rust.md）。範囲外なら `unwrap_or(&[])` で空スライスとして扱い、
        // `starts_with` が自然に `false` を返す（fail-closed）。
        let rest = self.bytes.get(self.pos..).unwrap_or(&[]);
        if rest.starts_with(b"true") {
            self.pos += 4;
            Ok(JsonValue::Bool(true))
        } else if rest.starts_with(b"false") {
            self.pos += 5;
            Ok(JsonValue::Bool(false))
        } else {
            Err(JsonError)
        }
    }

    fn parse_null(&mut self) -> Result<JsonValue, JsonError> {
        let rest = self.bytes.get(self.pos..).unwrap_or(&[]);
        if rest.starts_with(b"null") {
            self.pos += 4;
            Ok(JsonValue::Null)
        } else {
            Err(JsonError)
        }
    }

    /// RFC 8259 `number = [ "-" ] int [ frac ] [ exp ]` に沿って明示的に検証する。
    /// `int = "0" / ( digit1-9 *DIGIT )` を `match` の分岐そのもので表現している
    /// ため、先頭ゼロの直後に数字が続く入力（`"01"` 等）は `Some(b'0')` 分岐が
    /// 1 桁しか消費しない構造上、後続の桁が未消費のまま残り
    /// `parse_json`（末尾ゴミ検査）または呼び出し元の走査で自然に拒否される
    /// （本関数内では桁数を数えない）。`frac`・`exp` はそれぞれ「区切り文字の後に
    /// 数字が 1 つも無い」場合（`"1."`・`"1e"`・`"1e+"` 等）を明示的に拒否する。
    /// `+` 符号始まり・`.` 始まり・`NaN`／`Infinity` は呼び出し元 `parse_value` の
    /// 分岐条件（`Some(b'-') | Some(b'0'..=b'9')` のときのみ本関数へ到達）により
    /// 構造的に到達しないため、ここでは扱わない（Issue #732）。
    fn parse_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.pos;
        let negative = self.peek() == Some(b'-');
        if negative {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(JsonError),
        }
        // 小数点・指数部の有無を記録する（両方とも不在の場合のみ整数リテラルとして
        // `u64`／`i64` の無損失表現を試みる。丸めを伴う `f64` 変換は最後の手段）。
        let mut is_integer_literal = true;
        if self.peek() == Some(b'.') {
            is_integer_literal = false;
            self.pos += 1;
            let frac_start = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == frac_start {
                return Err(JsonError);
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_integer_literal = false;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            let exp_start = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == exp_start {
                return Err(JsonError);
            }
        }
        if self.pos - start > 64 {
            return Err(JsonError);
        }
        let Some(slice) = self.bytes.get(start..self.pos) else {
            return Err(JsonError);
        };
        let Ok(text) = std::str::from_utf8(slice) else {
            return Err(JsonError);
        };
        if is_integer_literal {
            // 小数点・指数部を含まない整数リテラル。`f64` へ変換する前に
            // `u64`／`i64` へのパースを試み、範囲内なら無損失表現で保持する
            // （PR #823 レビュー指摘: 2^53 を超える整数が `f64` の丸めで
            // 別の整数へエイリアスし、`id` 上限検査等をすり抜ける問題への対応）。
            // オーバーフローのみ `Float` へフォールバックする（RFC 8259 上は
            // 有効な数値のため受理自体は継続する）。
            if negative {
                if let Ok(n) = text.parse::<i64>() {
                    return Ok(JsonValue::Number(JsonNumber::NegInt(n)));
                }
            } else if let Ok(n) = text.parse::<u64>() {
                return Ok(JsonValue::Number(JsonNumber::PosInt(n)));
            }
        }
        text.parse::<f64>()
            .map(|f| {
                JsonValue::Number(JsonNumber::Float {
                    value: f,
                    text: Box::from(text),
                })
            })
            .map_err(|_| JsonError)
    }
}

/// `s` 全体を単一の JSON 値としてパースする（末尾に余分な非空白文字があれば拒否する。
/// 上限は [`MAX_JSON_DEPTH`]・[`MAX_JSON_STRING_CHARS`]・[`MAX_JSON_CONTAINER_ITEMS`]）。
pub fn parse_json(s: &str) -> Result<JsonValue, JsonError> {
    let mut parser = JsonParser::new(s);
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(JsonError);
    }
    Ok(value)
}

/// `JSON`／`JSONB` 列（TABLE-14・TASK-198、Issue #889）1 件あたりの最大バイト長。
/// `row_codec::encode_scalar_columns` は TEXT 列と同じ枠（presence 1 バイト +
/// `u32 LE` 長 4 バイト + UTF-8 本体）でこの値をスカラーペイロードへ書き込むため、
/// 本定数はそのフレーミング分（`row_codec::SCALAR_TEXT_ENTRY_OVERHEAD`）を差し引いた
/// `row_codec::MAX_SCALAR_PAYLOAD_LEN` 以下に定義する（`row_codec.rs` の `const`
/// アサーションで機械検証）。これにより、この列 1 個だけが対象行のスカラー
/// ペイロードを占める場合、[`validate_json_column_text`]／[`canonicalize_jsonb_text`]
/// を通過した入力は `encode_scalar_columns` で確実に格納できる（PR #1014 レビュー
/// 指摘対応。旧定義は `row_codec::MAX_TEXT_FIELD_LEN` と同値だったため、ちょうど
/// 上限の入力がフレーミング分だけ `MAX_SCALAR_PAYLOAD_LEN` を超え、束縛層
/// （`sql::parser::bind_json_literal`）を通過した後に `encode_scalar_columns` 側で
/// 拒否され得た）。同一行に他のスカラー列がある場合の累計超過は `encode_scalar_columns`
/// が持つ TEXT 列と共通の契約であり、本定数は単一列分の収容のみを保証する。
pub const MAX_JSON_FIELD_LEN: usize = (crate::row_codec::MAX_SCALAR_PAYLOAD_LEN
    - crate::row_codec::SCALAR_TEXT_ENTRY_OVERHEAD) as usize;

/// [`validate_json_column_text`]／[`canonicalize_jsonb_text`] の失敗を表す分類済み
/// エラー（Issue #889 D1）。private spec（TABLE-14・TASK-198）の受理規則違反は
/// 新規 `wire_code` を追加せず既存分類（`42601`／`54000`）へ写像する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonColumnError {
    /// [`MAX_JSON_FIELD_LEN`] 超過（`parse_json` を呼ぶ**前**にバイト長で判定する）。
    TooLong,
    /// 構文不正・重複キー・非 RFC 8259 数値・深さ/要素数超過（[`JsonError`] を包む）。
    Invalid,
}

impl ClassifiedError for JsonColumnError {
    fn error_class(&self) -> ErrorClass {
        match self {
            JsonColumnError::TooLong => ErrorClass::PayloadTooLarge,
            JsonColumnError::Invalid => ErrorClass::UnsupportedSqlSyntax,
        }
    }

    fn client_message(&self) -> String {
        match self {
            JsonColumnError::TooLong => "JSON value exceeds maximum length".to_string(),
            JsonColumnError::Invalid => "invalid JSON".to_string(),
        }
    }
}

impl JsonColumnError {
    /// SQLSTATE 風 `wire_code`（[`ClassifiedError`] へ委譲。実装型ごとに再定義しない）。
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }
}

/// `JSON` 列（正規化しない）向けの検証: 総バイト長を [`parse_json`] を呼ぶ**前**に
/// 判定してから構文検証する。呼び出し元（`row_codec` の encode チョークポイント・
/// SQL/NoSQL 表層の束縛層）はこれを通過した `s` をそのまま格納する契約とする
/// （Issue #889 D2「格納時検証の単一チョークポイント」）。
pub fn validate_json_column_text(s: &str) -> Result<(), JsonColumnError> {
    if s.len() > MAX_JSON_FIELD_LEN {
        return Err(JsonColumnError::TooLong);
    }
    parse_json(s).map_err(|_| JsonColumnError::Invalid)?;
    Ok(())
}

/// `JSONB` 列向けの正規化: 検証後に [`write_canonical`] で再シリアライズした文字列を
/// 返す（キーはキー文字列の昇順・空白なし。数値は [`JsonNumber`] の表現をそのまま
/// 経由するため往復ずれが生じない）。エスケープ展開で正規化後に伸びうるため、
/// 正規化後の長さも再判定する。
pub fn canonicalize_jsonb_text(s: &str) -> Result<String, JsonColumnError> {
    if s.len() > MAX_JSON_FIELD_LEN {
        return Err(JsonColumnError::TooLong);
    }
    let value = parse_json(s).map_err(|_| JsonColumnError::Invalid)?;
    let mut out = String::new();
    write_canonical(&value, &mut out);
    if out.len() > MAX_JSON_FIELD_LEN {
        return Err(JsonColumnError::TooLong);
    }
    Ok(out)
}

/// `v` を正規化 JSON テキストとして `out` へ追記する（唯一の正規化シリアライザ。
/// `JSONB` 列格納時の正規化・NoSQL 応答の再シリアライズがこれを共有する）。
/// オブジェクトのキーは [`JsonValue::Object`] が `BTreeMap` であるため既にキー
/// 文字列の昇順で走査され、要素間・キーと値の間に空白を挟まない。
pub fn write_canonical(v: &JsonValue, out: &mut String) {
    match v {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Number(n) => write_canonical_number(n, out),
        JsonValue::String(s) => write_canonical_string(s, out),
        JsonValue::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        JsonValue::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_string(k, out);
                out.push(':');
                write_canonical(v, out);
            }
            out.push('}');
        }
    }
}

/// [`JsonNumber`] を正規化テキストへ書き出す。整数 variant は 10 進整数として、
/// `Float` variant はパース時点の生リテラル文字列（`text`）をそのまま出力する
/// （`f64` を経由した再フォーマットは往復ずれ・表層横断 `content_hash` の不一致を
/// 招くため避ける。`JsonNumber::Float` のドキュメンテーションコメント参照）。
fn write_canonical_number(n: &JsonNumber, out: &mut String) {
    match n {
        JsonNumber::PosInt(v) => out.push_str(&v.to_string()),
        JsonNumber::NegInt(v) => out.push_str(&v.to_string()),
        JsonNumber::Float { text, .. } => out.push_str(text),
    }
}

/// JSON／JSONB 列を UNIQUE 制約の対象型として扱う際（TABLE-16・TASK-204、
/// Issue #1073）の**値としての等価正規化**テキストを返す。[`write_canonical`]
/// （`JSONB` 列格納時の正規化。パース時点の生リテラルを保持する）とは異なり、
/// 数値をキー順・空白だけでなく**値そのもの**として正規化する（`1`・`1.0`・
/// `1e0`・`10e-1` を同一に、`-0`・`0`・`-0.0`・`0e5` を同一のゼロに扱う。
/// PostgreSQL の `jsonb` の数値等価に揃える設計判断）。この関数が返すテキストは
/// 再パース可能な JSON 文字列であることを保証しない（等価性判定専用の内部
/// キー用途に限定するため）。
///
/// [`crate::constraint::push_canonical_component`] が UNIQUE 制約・主キーの
/// 正準キーバイト列を組み立てる唯一の呼び出し元（JSON／JSONB 型を一意キーの
/// 対象として許可するのはこの Issue が初めてであり、`ColumnType::
/// is_unique_constraint_allowed` 経由でのみ到達する）。将来 TABLE-14 の複合型
/// 等価述語（`=`）を実装する際も、値としての等価判定はこの関数を再利用すべき
/// 唯一の正準形とする（判断を二重に持たない）。
///
/// 格納済みの値が壊れている（`encode_scalar_columns` 側の検証を経由していない
/// 内部矛盾）場合は [`JsonColumnError::Invalid`] を返す（encode 時に検証済み
/// のため通常は到達しない。到達しても値を誤ってゼロ等へ縮退させず fail-closed
/// に拒否する）。
pub(crate) fn canonical_equality_text(s: &str) -> Result<String, JsonColumnError> {
    if s.len() > MAX_JSON_FIELD_LEN {
        return Err(JsonColumnError::TooLong);
    }
    let value = parse_json(s).map_err(|_| JsonColumnError::Invalid)?;
    let mut out = String::new();
    write_equality_canonical(&value, &mut out)?;
    Ok(out)
}

/// [`canonical_equality_text`] が使う再帰シリアライザ（[`write_canonical`] と
/// 構造は同じだが、数値の書き出しに [`write_equality_number`] を使う点のみ
/// 異なる）。
fn write_equality_canonical(v: &JsonValue, out: &mut String) -> Result<(), JsonColumnError> {
    match v {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Number(n) => write_equality_number(n, out)?,
        JsonValue::String(s) => write_canonical_string(s, out),
        JsonValue::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_equality_canonical(item, out)?;
            }
            out.push(']');
        }
        JsonValue::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_string(k, out);
                out.push(':');
                write_equality_canonical(v, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// [`JsonNumber`] を値としての正規化テキスト（`[-]<仮数>e<指数>`。ゼロは常に
/// 符号無しの `"0"`）へ書き出す（[`canonical_equality_text`] 専用）。
fn write_equality_number(n: &JsonNumber, out: &mut String) -> Result<(), JsonColumnError> {
    let (neg, mantissa, exp_neg, exponent) = normalize_json_number(n)?;
    if mantissa == "0" {
        // ゼロは符号・指数を問わず単一の表現に統一する（`-0`／`0.0`／`0e5` 等）。
        out.push('0');
        return Ok(());
    }
    if neg {
        out.push('-');
    }
    out.push_str(&mantissa);
    out.push('e');
    if exp_neg {
        out.push('-');
    }
    out.push_str(&exponent);
    Ok(())
}

/// `n` を「符号・仮数（先頭ゼロを含まず、ゼロでない限り末尾ゼロも含まない
/// 10 進数字列）・指数の符号・指数（先頭ゼロを含まない 10 進数字列）」へ
/// 分解する。整数 variant（`PosInt`／`NegInt`）はそのまま仮数として扱う
/// （指数 0）。`Float` はパース時点の生リテラル文字列（[`decompose_number_text`]
/// で構文要素へ分解）から、整数部・小数部を連結した数字列の先頭ゼロを除去し、
/// 末尾ゼロを 1 桁除去するたびに指数へ `+1` する（値を保つ代数的に等価な変形。
/// `1.50` と `1.5` が同一の `(仮数, 指数)` に正規化される）。指数の加算は
/// 10 進文字列同士の符号付き加算（[`signed_decimal_add`]）で行う——リテラル
/// 長の上限（64 バイト）により指数の桁数が `i128` の範囲を超えうるため
/// （Issue #1073 D-d）。
fn normalize_json_number(n: &JsonNumber) -> Result<(bool, String, bool, String), JsonColumnError> {
    let (neg, int_digits, frac_digits, lit_exp_neg, lit_exp_digits): (
        bool,
        String,
        String,
        bool,
        String,
    ) = match n {
        JsonNumber::PosInt(v) => (false, v.to_string(), String::new(), false, "0".to_string()),
        JsonNumber::NegInt(v) => {
            if *v == 0 {
                // JSON の `-0` は値としてはゼロ（`as_f64` の符号ビット保持とは
                // 別の関心事。ここは「値としての等価」のための正規化）。
                (
                    false,
                    "0".to_string(),
                    String::new(),
                    false,
                    "0".to_string(),
                )
            } else {
                (
                    true,
                    v.unsigned_abs().to_string(),
                    String::new(),
                    false,
                    "0".to_string(),
                )
            }
        }
        JsonNumber::Float { text, .. } => {
            let (neg, int_d, frac_d, exp_neg, exp_d) =
                decompose_number_text(text).ok_or(JsonColumnError::Invalid)?;
            (
                neg,
                int_d.to_string(),
                frac_d.to_string(),
                exp_neg,
                strip_leading_zeros(exp_d).to_string(),
            )
        }
    };

    let frac_len = frac_digits.len();
    let mut digits = int_digits;
    digits.push_str(&frac_digits);
    let digits = strip_leading_zeros(&digits).to_string();
    if digits == "0" {
        return Ok((false, "0".to_string(), false, "0".to_string()));
    }
    let mantissa = digits.trim_end_matches('0');
    let mantissa = if mantissa.is_empty() { "0" } else { mantissa };
    let trailing_zeros = digits.len() - mantissa.len();
    // 末尾ゼロ 1 個を落とすごとに指数を +1 する補正。`frac_len` 分は小数点の
    // 桁移動として指数を -1 する（`push_canonical_component` の呼び出し元が
    // 期待する「値として等価なら同一キー」という契約の核）。`trailing_zeros`・
    // `frac_len` はいずれも [`MAX_JSON_FIELD_LEN`] に由来する数値リテラル
    // テキストの長さ（`i64::MAX` に遠く及ばない）に収まるが、untrusted 入力
    // 経由の値であるため `checked` 変換で明示的に扱う（coding-rust.md）。
    let trailing_zeros_i64 = i64::try_from(trailing_zeros).map_err(|_| JsonColumnError::Invalid)?;
    let frac_len_i64 = i64::try_from(frac_len).map_err(|_| JsonColumnError::Invalid)?;
    let delta = trailing_zeros_i64
        .checked_sub(frac_len_i64)
        .ok_or(JsonColumnError::Invalid)?;
    let (delta_neg, delta_digits) = if delta < 0 {
        (true, delta.unsigned_abs().to_string())
    } else {
        (false, delta.to_string())
    };
    let (exp_neg, exp_digits) =
        signed_decimal_add(lit_exp_neg, &lit_exp_digits, delta_neg, &delta_digits);
    Ok((neg, mantissa.to_string(), exp_neg, exp_digits))
}

/// リテラル `text`（[`JsonNumber::Float`] が保持する RFC 8259 準拠の数値
/// リテラル。文法は [`JsonParser::parse_number`] が既に検証済み）を
/// `(負号, 整数部の数字列, 小数部の数字列, 指数の負号, 指数部の数字列)` へ
/// 分解する。`parse_number` と同じ文法を再解析するだけの内部ヘルパーであり、
/// 呼び出し元が保持する不変条件（構文検証済みのテキストのみ渡される）が破れて
/// いた場合のみ `None` を返す（fail-closed。呼び出し元 [`normalize_json_number`]
/// が `JsonColumnError::Invalid` へ変換する）。
fn decompose_number_text(text: &str) -> Option<(bool, &str, &str, bool, &str)> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let neg = bytes.first() == Some(&b'-');
    if neg {
        i += 1;
    }
    let int_start = i;
    while matches!(bytes.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    let int_digits = text.get(int_start..i)?;
    if int_digits.is_empty() {
        return None;
    }
    let mut frac_digits = "";
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let frac_start = i;
        while matches!(bytes.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        frac_digits = text.get(frac_start..i)?;
        if frac_digits.is_empty() {
            return None;
        }
    }
    let mut exp_neg = false;
    let mut exp_digits = "";
    if matches!(bytes.get(i), Some(b'e') | Some(b'E')) {
        i += 1;
        match bytes.get(i) {
            Some(b'+') => i += 1,
            Some(b'-') => {
                exp_neg = true;
                i += 1;
            }
            _ => {}
        }
        let exp_start = i;
        while matches!(bytes.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        exp_digits = text.get(exp_start..i)?;
        if exp_digits.is_empty() {
            return None;
        }
    }
    if i != bytes.len() {
        return None;
    }
    Some((neg, int_digits, frac_digits, exp_neg, exp_digits))
}

/// 先頭ゼロを除去した非負の 10 進数字列を返す（全桁ゼロなら `"0"` の 1 桁に
/// 正規化する）。空文字列は呼び出し元の不変条件（整数部・指数部は文法上
/// 非空）により渡らない前提だが、渡っても `"0"` として扱う（fail-closed に
/// panic しない）。
fn strip_leading_zeros(s: &str) -> &str {
    let trimmed = s.trim_start_matches('0');
    if trimmed.is_empty() {
        "0"
    } else {
        trimmed
    }
}

/// 2 個の非負 10 進数字列（[`strip_leading_zeros`] 済み）の大小を比較する
/// （同じ桁数なら辞書式比較が数値比較と一致する）。
fn digits_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Equal => a.cmp(b),
        other => other,
    }
}

/// 2 個の非負 10 進数字列を筆算加算する（`i128` を経由しない任意精度加算。
/// 指数がリテラル長上限〔64 バイト〕由来で `i128` に収まらない場合に備える。
/// Issue #1073 D-d）。
fn digits_add(a: &str, b: &str) -> String {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut result: Vec<u8> = Vec::new();
    let mut carry: u8 = 0;
    let mut ai = a_bytes.len();
    let mut bi = b_bytes.len();
    while ai > 0 || bi > 0 || carry > 0 {
        let da = if ai > 0 {
            ai -= 1;
            a_bytes.get(ai).copied().unwrap_or(b'0').wrapping_sub(b'0')
        } else {
            0
        };
        let db = if bi > 0 {
            bi -= 1;
            b_bytes.get(bi).copied().unwrap_or(b'0').wrapping_sub(b'0')
        } else {
            0
        };
        let sum = da + db + carry;
        result.push(b'0' + (sum % 10));
        carry = sum / 10;
    }
    result.reverse();
    let s = String::from_utf8(result).unwrap_or_else(|_| "0".to_string());
    strip_leading_zeros(&s).to_string()
}

/// `a - b`（`a >= b` を呼び出し元が保証する非負 10 進数字列同士の筆算減算。
/// [`digits_add`] と対になる任意精度演算）。
fn digits_sub(a: &str, b: &str) -> String {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut result: Vec<u8> = Vec::new();
    let mut borrow: i16 = 0;
    let mut ai = a_bytes.len();
    let mut bi = b_bytes.len();
    while ai > 0 {
        ai -= 1;
        let da = i16::from(a_bytes.get(ai).copied().unwrap_or(b'0').wrapping_sub(b'0'));
        let db = if bi > 0 {
            bi -= 1;
            i16::from(b_bytes.get(bi).copied().unwrap_or(b'0').wrapping_sub(b'0'))
        } else {
            0
        };
        let mut diff = da - db - borrow;
        if diff < 0 {
            diff += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        result.push(b'0' + diff as u8);
    }
    result.reverse();
    let s = String::from_utf8(result).unwrap_or_else(|_| "0".to_string());
    strip_leading_zeros(&s).to_string()
}

/// 符号付き 10 進数（符号＋非負数字列で表現）同士の加算。同符号は数字列を
/// 加算するだけ、異符号は絶対値の大小で減算対象を決める（結果ゼロは常に
/// 符号無し）。[`normalize_json_number`] がリテラル指数と末尾ゼロ補正量を
/// 合成する唯一の呼び出し元。
fn signed_decimal_add(a_neg: bool, a: &str, b_neg: bool, b: &str) -> (bool, String) {
    if a_neg == b_neg {
        let sum = digits_add(a, b);
        let neg = a_neg && sum != "0";
        (neg, sum)
    } else {
        match digits_cmp(a, b) {
            std::cmp::Ordering::Equal => (false, "0".to_string()),
            std::cmp::Ordering::Greater => {
                let diff = digits_sub(a, b);
                (a_neg && diff != "0", diff)
            }
            std::cmp::Ordering::Less => {
                let diff = digits_sub(b, a);
                (b_neg && diff != "0", diff)
            }
        }
    }
}

/// RFC 8259 準拠の最小エスケーパ（自作。wire-server の `error_body` エスケーパへは
/// engine から依存できないため独立実装とする）。`"`・`\`・U+0000〜U+001F を
/// エスケープし、それ以外（非 ASCII の生 UTF-8 を含む）はそのまま出力する。
fn write_canonical_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// JSON パス参照（Issue #889 D5）の 1 段。SQL/NoSQL 表層への構文露出は行わず、
/// engine の Rust API に限定した最小形（キー参照・配列添字参照）とする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPathStep {
    Key(String),
    Index(usize),
}

/// パス参照 API の失敗（ステップ数上限超過）を表すマーカー型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonPathError;

impl JsonPathError {
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }
}

impl ClassifiedError for JsonPathError {
    fn error_class(&self) -> ErrorClass {
        ErrorClass::UnsupportedSqlSyntax
    }

    fn client_message(&self) -> String {
        "invalid JSON path".to_string()
    }
}

/// 検証済みの JSON パス（ステップ数は [`MAX_JSON_DEPTH`] 以下。値のネスト深さ上限と
/// 揃え、パス自体が DoS の温床にならないようにする）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPath {
    steps: Vec<JsonPathStep>,
}

impl JsonPath {
    pub fn new(steps: Vec<JsonPathStep>) -> Result<Self, JsonPathError> {
        if steps.len() > MAX_JSON_DEPTH {
            return Err(JsonPathError);
        }
        Ok(Self { steps })
    }

    pub fn steps(&self) -> &[JsonPathStep] {
        &self.steps
    }
}

/// `v` から `path` が指す部分値への参照を返す（キー不在・型不一致・範囲外は
/// `None`。untrusted 入力経路のため添字アクセスではなく `get()` を使う）。
pub fn extract_path<'a>(v: &'a JsonValue, path: &JsonPath) -> Option<&'a JsonValue> {
    let mut cur = v;
    for step in path.steps() {
        cur = match (cur, step) {
            (JsonValue::Object(map), JsonPathStep::Key(k)) => map.get(k)?,
            (JsonValue::Array(items), JsonPathStep::Index(i)) => items.get(*i)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// 格納テキスト `stored`（`JSON`／`JSONB` 列の値）をパースし、`path` が指す部分値を
/// 正規化テキストで返す（`None` はキー不在・型不一致・範囲外。パース失敗は格納
/// 契約違反のため呼び出し元の内部エラーとして扱う）。
pub fn extract_path_text(stored: &str, path: &JsonPath) -> Result<Option<String>, JsonColumnError> {
    let value = parse_json(stored).map_err(|_| JsonColumnError::Invalid)?;
    Ok(extract_path(&value, path).map(|v| {
        let mut out = String::new();
        write_canonical(v, &mut out);
        out
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_rejects_excess_nesting_depth() {
        let mut s = String::new();
        for _ in 0..(MAX_JSON_DEPTH + 4) {
            s.push('[');
        }
        for _ in 0..(MAX_JSON_DEPTH + 4) {
            s.push(']');
        }
        assert_eq!(parse_json(&s).unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_rejects_trailing_garbage() {
        assert_eq!(parse_json("{}garbage").unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_handles_escaped_unicode() {
        let value = parse_json("\"\\u0041\\u0042\"").unwrap();
        assert_eq!(value, JsonValue::String("AB".to_string()));
    }

    // 回帰テスト（codex-review PR #252 P2 指摘対応）: 正規のサロゲートペアは
    // 補助平面のコードポイント 1 個へ復号され、孤立サロゲート（相方を伴わない
    // 高位・低位サロゲート）は破損文字列を U+FFFD へ丸めて返さず fail-closed に
    // 拒否する。
    #[test]
    fn parse_json_decodes_surrogate_pair_to_supplementary_plane_char() {
        // U+1F600 (😀) の UTF-16 サロゲートペア表現。
        let value = parse_json("\"\\ud83d\\ude00\"").unwrap();
        assert_eq!(value, JsonValue::String("\u{1f600}".to_string()));
    }

    #[test]
    fn parse_json_rejects_isolated_high_surrogate() {
        assert_eq!(parse_json("\"\\ud800\"").unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_rejects_isolated_low_surrogate() {
        assert_eq!(parse_json("\"\\udc00\"").unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_rejects_high_surrogate_not_followed_by_low_surrogate() {
        assert_eq!(parse_json("\"\\ud800\\u0041\"").unwrap_err(), JsonError);
    }

    // 重複キー拒否（Issue #732・TASK-172・NOSQL-8）: 後勝ちで無警告に上書きせず
    // fail-closed に拒否する。トップレベル・ネスト先いずれも対象。
    #[test]
    fn parse_json_rejects_duplicate_key_at_top_level() {
        let err = parse_json(r#"{"a":1,"a":2}"#).unwrap_err();
        assert_eq!(err, JsonError);
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn parse_json_rejects_duplicate_key_in_nested_object() {
        assert_eq!(parse_json(r#"{"a":{"b":1,"b":2}}"#).unwrap_err(), JsonError);
    }

    #[test]
    fn parse_json_accepts_non_duplicate_keys() {
        let value = parse_json(r#"{"a":1,"b":2}"#).unwrap();
        match value {
            JsonValue::Object(map) => {
                assert_eq!(
                    map.get("a"),
                    Some(&JsonValue::Number(JsonNumber::PosInt(1)))
                );
                assert_eq!(
                    map.get("b"),
                    Some(&JsonValue::Number(JsonNumber::PosInt(2)))
                );
            }
            _ => panic!("expected object"),
        }
    }

    // 数値構文の RFC 8259 厳格化（Issue #732）: 先頭ゼロ・小数部/指数部の数字欠落
    // 等の非準拠表記を拒否する。`+`／`.` 始まり・`NaN`／`Infinity` は
    // `parse_value` の分岐条件により既に構造的に拒否されるため、ここでは
    // その回帰防止のみ確認する。
    #[test]
    fn parse_json_rejects_non_rfc8259_numbers() {
        let rejected = [
            "01",
            "-01",
            "00",
            "+1",
            ".5",
            "1.",
            "1.e5",
            "1e",
            "1e+",
            "NaN",
            "Infinity",
            "-Infinity",
        ];
        for input in rejected {
            assert_eq!(
                parse_json(input).unwrap_err(),
                JsonError,
                "expected rejection for {input:?}"
            );
        }
    }

    /// `Float { value, text }` を組み立てるテスト専用ヘルパー（`text` は
    /// `input` そのものをそのまま保持する契約のため、期待値もこの形で構成する）。
    fn float_num(text: &str, value: f64) -> JsonNumber {
        JsonNumber::Float {
            value,
            text: Box::from(text),
        }
    }

    #[test]
    fn parse_json_accepts_rfc8259_numbers() {
        let accepted: &[(&str, JsonNumber)] = &[
            ("0", JsonNumber::PosInt(0)),
            ("-0", JsonNumber::NegInt(0)),
            ("0.5", float_num("0.5", 0.5)),
            ("-0.5", float_num("-0.5", -0.5)),
            ("123", JsonNumber::PosInt(123)),
            ("-123", JsonNumber::NegInt(-123)),
            ("1.5e10", float_num("1.5e10", 1.5e10)),
            ("1.5E-10", float_num("1.5E-10", 1.5e-10)),
            ("0e0", float_num("0e0", 0.0)),
            ("100", JsonNumber::PosInt(100)),
        ];
        for (input, expected) in accepted {
            let value =
                parse_json(input).unwrap_or_else(|_| panic!("expected accept for {input:?}"));
            assert_eq!(
                value,
                JsonValue::Number(expected.clone()),
                "input={input:?}"
            );
        }
    }

    // 精度を失う `f64` 変換の前に整数リテラルを判定する契約（PR #823 レビュー
    // 指摘対応）: 2^53 を超える整数リテラルが最近傍の偶数へ丸められて
    // 別の整数値へエイリアスすることなく、無損失な `u64`／`i64` 表現のまま
    // 保持されることを固定する。
    #[test]
    fn parse_json_number_preserves_integer_precision_beyond_f64_safe_range() {
        // 2^53 + 1。`f64` へ直接変換すると最近傍の偶数 2^53 へ丸められる値。
        let value = parse_json("9007199254740993").expect("valid JSON number");
        assert_eq!(
            value,
            JsonValue::Number(JsonNumber::PosInt(9_007_199_254_740_993))
        );

        // u64::MAX も無損失に保持される。
        let value = parse_json(&u64::MAX.to_string()).expect("valid JSON number");
        assert_eq!(value, JsonValue::Number(JsonNumber::PosInt(u64::MAX)));

        // 小数点を含む場合は整数であっても丸めを伴う Float 表現になる
        // （`id` 疑似列等の呼び出し元は `as_exact_u64()` が `None` を返すため
        // 拒否する。桁数上限 64 文字の範囲内で構成する）。
        let value = parse_json("9007199254740993.0").expect("valid JSON number");
        assert!(matches!(value, JsonValue::Number(JsonNumber::Float { .. })));
    }

    #[test]
    fn parse_json_number_rejection_reports_unsupported_sql_syntax_wire_code() {
        let err = parse_json("01").unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }
    #[test]
    fn neg_int_zero_converts_to_negative_zero_f64() {
        // JSON の `-0` は `NegInt(0)` として分類されるが、`f64` 化では SQL 表層の
        // `-0.0` と同じ符号ビットを保つ（content_hash の表層横断一致のため）。
        let v = JsonNumber::NegInt(0).as_f64();
        assert_eq!(v, 0.0);
        assert!(v.is_sign_negative(), "-0 must map to -0.0, got {v:?}");
        assert!(JsonNumber::PosInt(0).as_f64().is_sign_positive());
        assert_eq!(JsonNumber::NegInt(-5).as_f64(), -5.0);
    }

    // `as_f32` の表層横断一貫性（Issue #771 レビュー指摘対応）: `str -> f64 ->
    // f32` の 2 回丸めではなく、保持した生リテラル文字列を SQL 表層と同一の
    // `str -> f32` 単一丸めで変換することを固定する。
    #[test]
    fn as_f32_matches_direct_str_parse_and_differs_from_f64_roundtrip() {
        // 1 + 2^-24。`f64` へ丸めると厳密に表現可能なためそのまま保持されるが、
        // さらに `f32` へ丸める（2 回目の丸め）と最近傍偶数の `1.0` になる。
        // 直接 `f32` としてパースすると 2^-24 の桁が失われず `1.0` より大きい
        // 値になり、2 回丸め経路とはビットパターンが異なる。
        let text = "1.0000000596046448";
        let value = parse_json(text).expect("valid JSON number");
        let JsonValue::Number(n) = value else {
            panic!("expected number");
        };

        let direct: f32 = text.parse().expect("text parses as f32");
        let double_rounded = n.as_f64() as f32;

        assert_eq!(
            n.as_f32().expect("finite").to_bits(),
            direct.to_bits(),
            "as_f32 must match single-rounding str->f32 parse"
        );
        assert_ne!(
            direct.to_bits(),
            double_rounded.to_bits(),
            "test fixture must actually exercise the double-rounding discrepancy"
        );
    }

    #[test]
    fn as_f32_preserves_negative_zero_sign() {
        let value = parse_json("-0").expect("valid JSON number");
        let JsonValue::Number(n) = value else {
            panic!("expected number");
        };
        let f = n.as_f32().expect("finite");
        assert_eq!(f, 0.0);
        assert!(f.is_sign_negative(), "-0 must map to -0.0f32, got {f:?}");
    }

    #[test]
    fn as_f32_rejects_literal_exceeding_f32_range() {
        // `1e39` は `f64` としては有限だが `f32` の表現範囲を超え `+inf` になる
        // （SQL 表層 `parse_vector_literal` の非有限判定と同じ境界）。
        let value = parse_json("1e39").expect("valid JSON number (finite as f64)");
        let JsonValue::Number(n) = value else {
            panic!("expected number");
        };
        assert_eq!(n.as_f32(), None);
    }

    #[test]
    fn as_f32_integer_variants_match_text_parse() {
        // 整数 variant（`PosInt`／`NegInt`）も、10 進整数テキストを直接 `f32`
        // としてパースした場合とビット単位で一致することを固定する（`f64` を
        // 経由しないため 2 回丸めの余地自体が無いが、念のため機械検証する）。
        let cases = [
            "9007199254740993",
            "-9007199254740993",
            &u64::MAX.to_string(),
        ];
        for text in cases {
            let value = parse_json(text).expect("valid JSON number");
            let JsonValue::Number(n) = value else {
                panic!("expected number for {text:?}");
            };
            let direct: f32 = text.parse().expect("text parses as f32");
            assert_eq!(
                n.as_f32().expect("finite").to_bits(),
                direct.to_bits(),
                "mismatch for {text:?}"
            );
        }
    }

    // --- Issue #889（TABLE-14・TASK-198）: JSON/JSONB 列向けヘルパーの単体テスト ---

    #[test]
    fn validate_json_column_text_accepts_valid_json() {
        assert!(validate_json_column_text(r#"{"a":1}"#).is_ok());
        assert!(validate_json_column_text("[1,2,3]").is_ok());
        assert!(validate_json_column_text("null").is_ok());
        assert!(validate_json_column_text("42").is_ok());
        assert!(validate_json_column_text(r#""s""#).is_ok());
    }

    #[test]
    fn validate_json_column_text_rejects_invalid_syntax() {
        let err = validate_json_column_text("not json").unwrap_err();
        assert_eq!(err, JsonColumnError::Invalid);
        assert_eq!(err.wire_code(), "42601");
    }

    // 総バイト長の判定が `parse_json` を呼ぶ**前**に行われることを固定する
    // （Issue #889 D2）。上限+1 バイトの、構文としても不正な入力
    // （閉じ括弧を欠く）を渡し、`Invalid`（構文エラー）ではなく `TooLong`
    // （長さエラー）が返ることで判定順序を検証する。
    #[test]
    fn validate_json_column_text_checks_length_before_parsing() {
        let oversized = format!("[{}", "1".repeat(MAX_JSON_FIELD_LEN));
        assert!(oversized.len() > MAX_JSON_FIELD_LEN);
        let err = validate_json_column_text(&oversized).unwrap_err();
        assert_eq!(err, JsonColumnError::TooLong);
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn validate_json_column_text_accepts_at_exact_length_limit() {
        // ちょうど上限バイト数の有効な JSON は受理される（off-by-one 検査）。
        // 単一の文字列リテラルは `MAX_JSON_STRING_CHARS`（別の上限。1 MiB）に
        // 抵触するため、5 要素の配列（各要素は上限未満の文字列）で目標
        // バイト数をちょうど組み立てる: `[` + 5 要素（`"`×2 + 本体） + 4 個の
        // `,` + `]`。
        let target = MAX_JSON_FIELD_LEN;
        let overhead = 1 + 1 + 4 + 5 * 2; // "[" + "]" + 4 commas + 5 pairs of quotes
        let content_total = target - overhead;
        let base = content_total / 5;
        let remainder = content_total % 5;
        let lens = [
            base,
            base,
            base,
            base,
            base + remainder, // 端数は最後の要素へ寄せる
        ];
        assert!(lens.iter().all(|&l| l < MAX_JSON_STRING_CHARS));
        let mut s = String::new();
        s.push('[');
        for (i, len) in lens.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('"');
            s.push_str(&"a".repeat(*len));
            s.push('"');
        }
        s.push(']');
        assert_eq!(s.len(), target, "constructed JSON length mismatch");
        assert!(validate_json_column_text(&s).is_ok(), "len={}", s.len());
    }

    #[test]
    fn canonicalize_jsonb_text_sorts_keys_and_strips_whitespace() {
        let canonical = canonicalize_jsonb_text(r#"{ "b": 2, "a": 1 }"#).expect("valid JSON");
        assert_eq!(canonical, r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn canonicalize_jsonb_text_is_idempotent() {
        let once = canonicalize_jsonb_text(r#"{"b":2,"a":1}"#).expect("valid JSON");
        let twice = canonicalize_jsonb_text(&once).expect("valid JSON");
        assert_eq!(once, twice);
    }

    #[test]
    fn canonicalize_jsonb_text_preserves_float_literal_text() {
        // 浮動小数はパース時点の生リテラル文字列をそのまま出力する
        // （`f64` を経由した再フォーマットによる往復ずれを避ける）。
        let canonical = canonicalize_jsonb_text("1.5000").expect("valid JSON");
        assert_eq!(canonical, "1.5000");
    }

    #[test]
    fn canonicalize_jsonb_text_checks_length_before_parsing() {
        let oversized = format!("[{}", "1".repeat(MAX_JSON_FIELD_LEN));
        let err = canonicalize_jsonb_text(&oversized).unwrap_err();
        assert_eq!(err, JsonColumnError::TooLong);
    }

    #[test]
    fn canonicalize_jsonb_text_rejects_invalid_syntax() {
        let err = canonicalize_jsonb_text("not json").unwrap_err();
        assert_eq!(err, JsonColumnError::Invalid);
    }

    #[test]
    fn write_canonical_escapes_control_characters_and_quotes() {
        let value = parse_json("\"a\\\"b\\nc\"").expect("valid JSON");
        let mut out = String::new();
        write_canonical(&value, &mut out);
        assert_eq!(out, "\"a\\\"b\\nc\"");
    }

    #[test]
    fn write_canonical_preserves_non_ascii_utf8() {
        let value = parse_json("\"\u{3042}\"").expect("valid JSON");
        let mut out = String::new();
        write_canonical(&value, &mut out);
        assert_eq!(out, "\"\u{3042}\"");
    }

    #[test]
    fn extract_path_resolves_nested_key_and_index() {
        let value = parse_json(r#"{"a":{"b":[10,20,30]}}"#).expect("valid JSON");
        let path = JsonPath::new(vec![
            JsonPathStep::Key("a".to_string()),
            JsonPathStep::Key("b".to_string()),
            JsonPathStep::Index(1),
        ])
        .expect("valid path");
        let found = extract_path(&value, &path).expect("path should resolve");
        assert_eq!(found, &JsonValue::Number(JsonNumber::PosInt(20)));
    }

    #[test]
    fn extract_path_returns_none_for_missing_key_or_type_mismatch_or_out_of_range() {
        let value = parse_json(r#"{"a":[1,2]}"#).expect("valid JSON");
        // キー不在。
        let missing_key = JsonPath::new(vec![JsonPathStep::Key("z".to_string())]).unwrap();
        assert_eq!(extract_path(&value, &missing_key), None);
        // 型不一致（オブジェクトへ Index、配列へ Key）。
        let type_mismatch = JsonPath::new(vec![
            JsonPathStep::Key("a".to_string()),
            JsonPathStep::Key("z".to_string()),
        ])
        .unwrap();
        assert_eq!(extract_path(&value, &type_mismatch), None);
        // 範囲外の添字。
        let out_of_range = JsonPath::new(vec![
            JsonPathStep::Key("a".to_string()),
            JsonPathStep::Index(99),
        ])
        .unwrap();
        assert_eq!(extract_path(&value, &out_of_range), None);
    }

    #[test]
    fn json_path_new_rejects_excess_step_count() {
        let steps = (0..=MAX_JSON_DEPTH)
            .map(|i| JsonPathStep::Key(i.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(steps.len(), MAX_JSON_DEPTH + 1);
        assert!(JsonPath::new(steps).is_err());
    }

    #[test]
    fn extract_path_text_returns_canonical_text_of_resolved_value() {
        let stored = r#"{"a":{"b":1}}"#;
        let path = JsonPath::new(vec![JsonPathStep::Key("a".to_string())]).unwrap();
        let text = extract_path_text(stored, &path)
            .expect("valid stored JSON")
            .expect("path should resolve");
        assert_eq!(text, r#"{"b":1}"#);
    }

    #[test]
    fn extract_path_text_returns_none_for_unresolved_path() {
        let stored = r#"{"a":1}"#;
        let path = JsonPath::new(vec![JsonPathStep::Key("z".to_string())]).unwrap();
        let result = extract_path_text(stored, &path).expect("valid stored JSON");
        assert_eq!(result, None);
    }

    // --- Issue #1073: `canonical_equality_text`（値としての等価正規化） -----

    /// キー順・空白の揺れは値としての等価に影響しない
    /// （`write_canonical` と同じ性質だが、数値も値として正規化する点が異なる）。
    #[test]
    fn canonical_equality_text_ignores_key_order_and_whitespace() {
        let a = canonical_equality_text(r#"{"a":1,"b":2}"#).expect("valid JSON");
        let b = canonical_equality_text(r#"{ "b": 2, "a": 1 }"#).expect("valid JSON");
        assert_eq!(a, b);
    }

    /// エスケープ表現の揺れは値としての等価に影響しない。
    #[test]
    fn canonical_equality_text_ignores_string_escape_representation() {
        let a = canonical_equality_text(r#""A""#).expect("valid JSON");
        let b = canonical_equality_text("\"\\u0041\"").expect("valid JSON");
        assert_eq!(a, b);
    }

    /// 数値表現の揺れ（整数・小数点・指数）は値として同一なら同じキーになる。
    #[test]
    fn canonical_equality_text_treats_equal_numeric_literals_as_identical() {
        let equal_to_one = ["1", "1.0", "1e0", "1E0", "10e-1", "0.1e1"];
        let expected = canonical_equality_text("1").expect("valid JSON");
        for literal in equal_to_one {
            let actual = canonical_equality_text(literal).expect("valid JSON");
            assert_eq!(actual, expected, "literal={literal:?}");
        }
    }

    /// `-0`／`0`／`-0.0`／`0e10` はいずれも単一のゼロ表現に正規化される。
    #[test]
    fn canonical_equality_text_treats_all_zero_representations_as_identical() {
        let zeros = ["0", "-0", "0.0", "-0.0", "0e10", "-0e10", "0.0e-5"];
        let expected = canonical_equality_text("0").expect("valid JSON");
        for literal in zeros {
            let actual = canonical_equality_text(literal).expect("valid JSON");
            assert_eq!(actual, expected, "literal={literal:?}");
        }
    }

    /// 異なる値（数値・文字列との型違い・配列要素順）は異なるキーになる。
    #[test]
    fn canonical_equality_text_distinguishes_different_values() {
        let one = canonical_equality_text("1").expect("valid JSON");
        let two = canonical_equality_text("2").expect("valid JSON");
        assert_ne!(one, two);

        let number = canonical_equality_text("1").expect("valid JSON");
        let string = canonical_equality_text(r#""1""#).expect("valid JSON");
        assert_ne!(number, string);

        let ab = canonical_equality_text("[1,2]").expect("valid JSON");
        let ba = canonical_equality_text("[2,1]").expect("valid JSON");
        assert_ne!(ab, ba);
    }

    /// 入れ子オブジェクトのキー順の揺れも値としての等価に影響しない。
    #[test]
    fn canonical_equality_text_ignores_nested_object_key_order() {
        let a = canonical_equality_text(r#"{"outer":{"a":1,"b":2}}"#).expect("valid JSON");
        let b = canonical_equality_text(r#"{"outer":{"b":2,"a":1}}"#).expect("valid JSON");
        assert_eq!(a, b);
    }

    /// リテラル長上限（64 バイト）に近い巨大な指数同士も、値として等価なら
    /// 同一キーになり、指数が 1 だけずれたものは別のキーになる
    /// （`i128` に収まらない指数を 10 進文字列の任意精度加算で扱う経路の検証。
    /// Issue #1073 D-d）。
    #[test]
    fn canonical_equality_text_handles_exponents_beyond_i128_range() {
        // `i128` の桁数上限（約 38 桁）を超える 40 桁の指数を、`u128` 等の数値
        // 型を経由せず 10 進文字列だけで組み立てる（テスト自体が `i128`／
        // `u128` の範囲に依存しないようにするため）。
        let exp = "1".to_string() + &"0".repeat(39); // 10^39（40 桁）
        let exp_minus_one = "9".repeat(39); // 10^39 - 1（39 桁の 9）
        let exp_plus_one = "1".to_string() + &"0".repeat(38) + "1"; // 10^39 + 1

        let a = format!("1e{exp}");
        let a_canonical = canonical_equality_text(&a).expect("valid JSON");

        // 同じ値を「仮数 10・指数を 1 減らす」形で表現する（値として等価）。
        let b = format!("10e{exp_minus_one}");
        let b_canonical = canonical_equality_text(&b).expect("valid JSON");
        assert_eq!(a_canonical, b_canonical, "a={a:?} b={b:?}");

        // 指数を 1 だけずらすと異なる値になる。
        let c = format!("1e{exp_plus_one}");
        let c_canonical = canonical_equality_text(&c).expect("valid JSON");
        assert_ne!(a_canonical, c_canonical);
    }

    /// 非常に小さい指数（大きく負の指数）も同じ経路で正しく正規化される。
    #[test]
    fn canonical_equality_text_handles_large_negative_exponents() {
        let a = canonical_equality_text("1e-30").expect("valid JSON");
        let b = canonical_equality_text("10e-31").expect("valid JSON");
        assert_eq!(a, b);
        let c = canonical_equality_text("1e-29").expect("valid JSON");
        assert_ne!(a, c);
    }

    /// 無効な JSON は `Invalid` として拒否される。
    #[test]
    fn canonical_equality_text_rejects_invalid_syntax() {
        let err = canonical_equality_text("not json").unwrap_err();
        assert_eq!(err, JsonColumnError::Invalid);
    }

    /// 上限超過は構文解析前に `TooLong` として拒否される。
    #[test]
    fn canonical_equality_text_checks_length_before_parsing() {
        let oversized = format!("[{}", "1".repeat(MAX_JSON_FIELD_LEN));
        let err = canonical_equality_text(&oversized).unwrap_err();
        assert_eq!(err, JsonColumnError::TooLong);
    }
}
