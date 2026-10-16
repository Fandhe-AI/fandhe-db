//! SQL/JSON path（`jsonpath`）の評価器（Issue #2184・TABLE-25 ポインタ）。
//!
//! 責務境界: 解析済みの [`SqlJsonPath`]（[`super::parse_jsonpath`] の成功値）を jsonb 値
//! （[`JsonValue`]）へ適用し、結果の項目列・存在判定・真偽判定を返すところまで。SQL 表層
//! への配線（`jsonpath` 型の入出力・`jsonb_path_*`〔Issue #2185〕・`@?`／`@@`〔#2186〕・
//! `JSON_EXISTS` 等〔#2187〕）は呼び出し側の後続 Issue が担い、本モジュールは engine 内の
//! Rust API [`jsonpath_query`]・[`jsonpath_exists`]・[`jsonpath_match`] だけを公開する。
//! 評価器は渡された値だけを読む純関数で、ストレージ・カタログ・テナント情報に触れない。
//!
//! 意味論は PostgreSQL 17 の `jsonpath_exec.c` に合わせる（実装は自前）。
//!
//! - **lax／strict**: lax は配列の自動展開（アクセサ・フィルタ・多くの項目メソッド・比較の
//!   両辺）と非配列の単一要素配列扱いを行い、構造エラー（キー欠落・型違いのアクセサ・添字
//!   範囲外）を抑止して空列にする。strict は構造エラーを `2203x` 系で送出する。
//! - **エラーの 3 層**: (a) lax は構造エラーだけを抑止（算術の非数値・0 除算は lax でも送出）。
//!   (b) `silent` は抑止可能な評価エラー全般を抑止する。(c) 述語（比較・`starts with`・
//!   `exists`）のオペランド内のエラーは両モードとも unknown になる。**抑止不可**: 上限超過
//!   `54000`・未定義変数 `42704`・`vars` が非オブジェクト `22023`・未対応機能 `0A000`
//!   （fail-closed）。
//! - **フィルタ**: 述語内のエラーは unknown になる。フィルタは true のみ通過し、
//!   `is unknown`・`!`・`&&`・`||` は Kleene 論理。
//! - **上限**（fail-closed）: 評価ステップ数と再帰深さを [`JsonPathLimits`] で制限し、超過は
//!   `54000`。評価器が新たに作る値（`.keyvalue()`）は `MAX_JSON_DEPTH` を、項目列は
//!   [`MAX_SEQUENCE_ITEMS`] を検査する。`.**` の JSON 走査は再帰でなく明示スタックで行う。
//!
//! 意図的な差分（`docs/design/json-expr-type-mapping.md` に記録）: 数値演算は `i128`／
//! `scale <= 38` に収まる範囲（超過は `22003`）、`.keyvalue()` の `id`、datetime 系メソッドと
//! `like_regex` は `0A000`、文字列比較はバイト順、エラー文言は固定の英語文で PostgreSQL と
//! 一致させない（入力の断片を含めない）。

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use super::ast::{
    AccessorOp, AnyLevel, ArithOp, CompareOp, Method, PathMode, PathNode, PathNumeric, SqlJsonPath,
    StartsWithOperand, UnaryOp,
};
use super::numeric as num;
use crate::error_format::{ClassifiedError, ErrorClass};
use crate::json::{cmp_json_numbers, JsonNumber, JsonValue, MAX_JSON_DEPTH};
use crate::jsonpath_limits::JsonPathLimits;
use crate::numeric::Decimal;

/// 評価中に保持する項目列（フィルタ対象・オペランド列・結果列）の最大長。ステップ上限とは
/// 別に、メモリを有界にするための安全弁（超過は `54000`）。
pub const MAX_SEQUENCE_ITEMS: usize = 1_048_576;

/// 1 回の評価で新規に確保してよい値のバイト数の累計上限（複製・文字列生成の合計。
/// ノード数課金だけでは巨大文字列の複製を抑えられないための安全弁。超過は `54000`）。
pub const MAX_EVAL_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

/// 値 1 ノードあたりの固定費として見積もるバイト数（バイト予算の概算用）。
const NODE_COST_BYTES: u64 = 32;

/// 評価の失敗。`wire_code()` は PostgreSQL 17 の SQLSTATE に合わせる。`client_message` は
/// 固定の英語文で、キー名・変数名・値・位置など入力の断片を含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonPathEvalError {
    /// strict でメンバーアクセサのキーが無い（`2203A`）。
    MemberNotFound,
    /// メンバーアクセサ・`.keyvalue()` がオブジェクト以外に適用された（`2203C`）。
    ObjectNotFound,
    /// 配列アクセサ・`.size()` が配列以外に適用された（`22039`）。
    ArrayNotFound,
    /// strict で配列添字が範囲外（`22033`）。
    SubscriptOutOfBounds,
    /// 配列添字が単一の数値でない（`22033`）。
    SubscriptNotNumeric,
    /// 配列添字が int4 の範囲外（`22033`）。
    SubscriptOutOfRange,
    /// 単項算術・`.abs()` 等が数値以外に適用された（`2203B`）。
    NumberRequired,
    /// 二項算術のオペランドが単一の数値でない（`22038`）。
    SingletonRequired,
    /// `jsonb_path_match` 相当で結果が単一の真偽値でない（`22038`）。
    BooleanResultRequired,
    /// 項目メソッドが対象外の型に適用された・変換が範囲外（`22036`）。
    NonNumericItem,
    /// 0 による除算・剰余（`22012`）。
    DivisionByZero,
    /// 数値が表現範囲外（`22003`）。本実装の算術は `i128`／`scale <= 38` に限る。
    NumericOutOfRange,
    /// 未定義の変数 `$name`（`42704`）。抑止不可。
    UndefinedVariable,
    /// `vars` がオブジェクトでない（`22023`）。抑止不可。
    VarsNotObject,
    /// `.decimal(p, s)` の p／s が範囲外（`22023`）。
    InvalidMethodArgument,
    /// ステップ数上限超過（`54000`）。抑止不可。
    StepLimitExceeded,
    /// 評価の再帰深さ上限超過（`54000`）。抑止不可。
    DepthLimitExceeded,
    /// 評価器が作る値・項目列の大きさ上限超過（`54000`）。抑止不可。
    ResultTooLarge,
    /// 未対応機能（datetime 系メソッド・`like_regex`。`0A000`）。抑止不可。
    Unsupported,
    /// 解析器が排除しているはずの不整合（`XX000`）。抑止不可。
    Internal,
}

impl JsonPathEvalError {
    /// SQLSTATE 風 `wire_code`（[`ClassifiedError`] へ委譲）。
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }

    /// `silent`（および述語オペランド内）で抑止される評価エラーか。`false` のものは常に
    /// 呼び出し元へ返る（fail-closed）。
    pub fn is_suppressible(&self) -> bool {
        matches!(
            self,
            JsonPathEvalError::MemberNotFound
                | JsonPathEvalError::ObjectNotFound
                | JsonPathEvalError::ArrayNotFound
                | JsonPathEvalError::SubscriptOutOfBounds
                | JsonPathEvalError::SubscriptNotNumeric
                | JsonPathEvalError::SubscriptOutOfRange
                | JsonPathEvalError::NumberRequired
                | JsonPathEvalError::SingletonRequired
                | JsonPathEvalError::NonNumericItem
                | JsonPathEvalError::InvalidMethodArgument
                | JsonPathEvalError::DivisionByZero
                | JsonPathEvalError::NumericOutOfRange
        )
    }
}

impl ClassifiedError for JsonPathEvalError {
    fn error_class(&self) -> ErrorClass {
        match self {
            JsonPathEvalError::MemberNotFound => ErrorClass::JsonMemberNotFound,
            JsonPathEvalError::ObjectNotFound => ErrorClass::JsonObjectNotFound,
            JsonPathEvalError::ArrayNotFound => ErrorClass::JsonArrayNotFound,
            JsonPathEvalError::SubscriptOutOfBounds
            | JsonPathEvalError::SubscriptNotNumeric
            | JsonPathEvalError::SubscriptOutOfRange => ErrorClass::JsonSubscriptInvalid,
            JsonPathEvalError::NumberRequired => ErrorClass::JsonNumberNotFound,
            JsonPathEvalError::SingletonRequired | JsonPathEvalError::BooleanResultRequired => {
                ErrorClass::SingletonJsonItemRequired
            }
            JsonPathEvalError::NonNumericItem => ErrorClass::NonNumericJsonItem,
            JsonPathEvalError::DivisionByZero => ErrorClass::DivisionByZero,
            JsonPathEvalError::NumericOutOfRange => ErrorClass::NumericOutOfRange,
            JsonPathEvalError::UndefinedVariable => ErrorClass::UndefinedObject,
            JsonPathEvalError::VarsNotObject | JsonPathEvalError::InvalidMethodArgument => {
                ErrorClass::InvalidParameterValue
            }
            JsonPathEvalError::StepLimitExceeded
            | JsonPathEvalError::DepthLimitExceeded
            | JsonPathEvalError::ResultTooLarge => ErrorClass::PayloadTooLarge,
            JsonPathEvalError::Unsupported => ErrorClass::FeatureNotSupported,
            JsonPathEvalError::Internal => ErrorClass::InternalError,
        }
    }

    fn client_message(&self) -> String {
        match self {
            JsonPathEvalError::MemberNotFound => "JSON object does not contain the requested key",
            JsonPathEvalError::ObjectNotFound => {
                "jsonpath member accessor can only be applied to an object"
            }
            JsonPathEvalError::ArrayNotFound => {
                "jsonpath array accessor can only be applied to an array"
            }
            JsonPathEvalError::SubscriptOutOfBounds => "jsonpath array subscript is out of bounds",
            JsonPathEvalError::SubscriptNotNumeric => {
                "jsonpath array subscript is not a single numeric value"
            }
            JsonPathEvalError::SubscriptOutOfRange => {
                "jsonpath array subscript is out of integer range"
            }
            JsonPathEvalError::NumberRequired => {
                "jsonpath numeric operator was applied to a non-numeric value"
            }
            JsonPathEvalError::SingletonRequired => {
                "jsonpath operand is not a single numeric value"
            }
            JsonPathEvalError::BooleanResultRequired => "single boolean result is expected",
            JsonPathEvalError::NonNumericItem => {
                "jsonpath item method cannot be applied to this value"
            }
            JsonPathEvalError::DivisionByZero => "division by zero",
            JsonPathEvalError::NumericOutOfRange => "jsonpath numeric value is out of range",
            JsonPathEvalError::UndefinedVariable => "could not find jsonpath variable",
            JsonPathEvalError::VarsNotObject => "\"vars\" argument is not an object",
            JsonPathEvalError::InvalidMethodArgument => {
                "jsonpath item method argument is out of range"
            }
            JsonPathEvalError::StepLimitExceeded => "jsonpath evaluation exceeds the step limit",
            JsonPathEvalError::DepthLimitExceeded => "jsonpath evaluation exceeds the depth limit",
            JsonPathEvalError::ResultTooLarge => "jsonpath evaluation result is too large",
            JsonPathEvalError::Unsupported => "jsonpath feature is not supported",
            JsonPathEvalError::Internal => "internal jsonpath evaluation error",
        }
        .to_string()
    }
}

/// 評価のオプション。`vars` は呼び出し側が `parse_json` 済みの `JsonValue` を渡す
/// （オブジェクトでなければ `22023`）。`silent` は `jsonb_path_*` の同名引数に相当する。
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonPathEvalOptions<'a> {
    vars: Option<&'a JsonValue>,
    silent: bool,
    limits: JsonPathLimits,
}

impl<'a> JsonPathEvalOptions<'a> {
    /// 既定（変数なし・非 silent・ハード上限）。
    pub fn new() -> Self {
        Self::default()
    }

    /// `$name` 参照の変数オブジェクトを設定する。
    pub fn with_vars(mut self, vars: &'a JsonValue) -> Self {
        self.vars = Some(vars);
        self
    }

    /// 抑止可能な評価エラーを空列／`None` にする。
    pub fn with_silent(mut self, silent: bool) -> Self {
        self.silent = silent;
        self
    }

    /// ステップ数・再帰深さの上限を設定する。
    pub fn with_limits(mut self, limits: JsonPathLimits) -> Self {
        self.limits = limits;
        self
    }
}

// ---------------------------------------------------------------------------
// 項目表現
// ---------------------------------------------------------------------------

/// 評価中の項目。入力文書の部分木は借用のまま運び、評価器が作る値（数値・文字列・
/// 真偽値・`.keyvalue()` の組）だけを所有する。`KeyValue` は 3 キー（`id`・`key`・
/// `value`）のオブジェクトを複製せずに表す。
#[derive(Debug, Clone)]
enum Item<'a> {
    Ref(&'a JsonValue),
    Null,
    Bool(bool),
    Num(JsonNumber),
    Str(Cow<'a, str>),
    KeyValue {
        id: i64,
        key: &'a str,
        value: &'a JsonValue,
    },
}

/// 比較・型判定で使う項目の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Null,
    Bool,
    Num,
    Str,
    Array,
    Object,
}

/// jsonb のオブジェクトの走査順（キー長の昇順、同長はバイト順）。PostgreSQL の jsonb が
/// 内部で持つ順序で、`.*`・`.**`・`.keyvalue()` の結果列の順序に影響する。
fn pg_key_order(a: &str, b: &str) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

impl<'a> Item<'a> {
    fn kind(&self) -> Kind {
        match self {
            Item::Ref(v) => match v {
                JsonValue::Null => Kind::Null,
                JsonValue::Bool(_) => Kind::Bool,
                JsonValue::Number(_) => Kind::Num,
                JsonValue::String(_) => Kind::Str,
                JsonValue::Array(_) => Kind::Array,
                JsonValue::Object(_) => Kind::Object,
            },
            Item::Null => Kind::Null,
            Item::Bool(_) => Kind::Bool,
            Item::Num(_) => Kind::Num,
            Item::Str(_) => Kind::Str,
            Item::KeyValue { .. } => Kind::Object,
        }
    }

    fn is_array(&self) -> bool {
        self.kind() == Kind::Array
    }

    fn is_container(&self) -> bool {
        matches!(self.kind(), Kind::Array | Kind::Object)
    }

    fn num(&self) -> Option<&JsonNumber> {
        match self {
            Item::Ref(JsonValue::Number(n)) | Item::Num(n) => Some(n),
            _ => None,
        }
    }

    fn str(&self) -> Option<&str> {
        match self {
            Item::Ref(JsonValue::String(s)) => Some(s.as_str()),
            Item::Str(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    fn array_elems(&self) -> Option<&'a [JsonValue]> {
        match self {
            Item::Ref(JsonValue::Array(v)) => Some(v.as_slice()),
            _ => None,
        }
    }

    /// オブジェクトのメンバー参照（`.key`）。
    fn object_get(&self, key: &str) -> Option<Item<'a>> {
        match self {
            Item::Ref(JsonValue::Object(m)) => m.get(key).map(Item::Ref),
            Item::KeyValue { id, key: k, value } => match key {
                "id" => Some(Item::Num(int_number(*id))),
                "key" => Some(Item::Str(Cow::Borrowed(k))),
                "value" => Some(Item::Ref(value)),
                _ => None,
            },
            _ => None,
        }
    }

    /// 子項目の数（コンテナ以外は `None`）。
    fn child_count(&self) -> Option<usize> {
        match self {
            Item::Ref(JsonValue::Array(v)) => Some(v.len()),
            Item::Ref(JsonValue::Object(m)) => Some(m.len()),
            Item::KeyValue { .. } => Some(3),
            _ => None,
        }
    }

    /// 子項目（配列は要素、オブジェクトは値を jsonb の走査順で）。確保は呼び出し側が
    /// [`Ev::charged_children`] で子の数だけ課金してから行う。
    fn children(&self) -> Option<Vec<Item<'a>>> {
        match self {
            Item::Ref(JsonValue::Array(v)) => Some(v.iter().map(Item::Ref).collect()),
            Item::Ref(JsonValue::Object(m)) => {
                let mut pairs: Vec<(&'a String, &'a JsonValue)> = m.iter().collect();
                pairs.sort_by(|a, b| pg_key_order(a.0, b.0));
                Some(pairs.into_iter().map(|(_, v)| Item::Ref(v)).collect())
            }
            Item::KeyValue { id, key, value } => Some(vec![
                Item::Num(int_number(*id)),
                Item::Str(Cow::Borrowed(key)),
                Item::Ref(value),
            ]),
            _ => None,
        }
    }

    fn type_name(&self) -> &'static str {
        match self.kind() {
            Kind::Null => "null",
            Kind::Bool => "boolean",
            Kind::Num => "number",
            Kind::Str => "string",
            Kind::Array => "array",
            Kind::Object => "object",
        }
    }
}

fn int_number(v: i64) -> JsonNumber {
    match u64::try_from(v) {
        Ok(u) => JsonNumber::PosInt(u),
        Err(_) => JsonNumber::NegInt(v),
    }
}

/// jsonpath の数値リテラル（解析器が正規化した 10 進テキスト）を [`JsonNumber`] にする。
fn literal_number(n: &PathNumeric) -> JsonNumber {
    let text = n.as_str();
    if let Ok(u) = text.parse::<u64>() {
        return JsonNumber::PosInt(u);
    }
    if let Ok(i) = text.parse::<i64>() {
        return JsonNumber::NegInt(i);
    }
    // 巨大な指数は `f64` では無限大になりうる。値の比較は桁文字列（`cmp_json_numbers`）が
    // 担い、`value` は `as_f64` の近似にしか使われない。
    let raw = JsonNumber::Float {
        value: text.parse::<f64>().unwrap_or(0.0),
        text: text.into(),
    };
    // `1e3` のような指数表記は、収まる範囲で正規の 10 進（`1000`）にそろえる。
    match num::from_json_number(&raw) {
        Some(d) => num::to_json_number(d),
        None => raw,
    }
}

fn decimal_number(d: Decimal) -> Item<'static> {
    Item::Num(num::to_json_number(d))
}

// ---------------------------------------------------------------------------
// 評価コンテキスト
// ---------------------------------------------------------------------------

/// 評価の失敗。`Thrown` は呼び出し元へ返すエラー、`Error` は PostgreSQL の `jperError`
/// （抑止されたエラー。述語・`exists` のオペランドでは unknown になる）。
#[derive(Debug, Clone, Copy)]
enum Fail {
    Thrown(JsonPathEvalError),
    Error,
}

type R<T> = Result<T, Fail>;

/// 評価結果（PostgreSQL の `jperOk`／`jperNotFound`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Res {
    Ok,
    NotFound,
}

/// 三値論理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Unknown,
}

fn hard(e: JsonPathEvalError) -> Fail {
    Fail::Thrown(e)
}

/// 項目列の書き込み先。`None` は「最初の 1 件で打ち切る」存在判定。
type Found<'f, 'a> = Option<&'f mut Vec<Item<'a>>>;

/// アクセサ連鎖の残り（PostgreSQL の `next` 項目）。`outer` は入れ子の式（`(expr).a`）が
/// 後ろに持つ連鎖で、スタック上に連なる。
#[derive(Clone, Copy)]
struct Next<'c, 'a> {
    ops: &'a [AccessorOp],
    outer: Option<&'c Next<'c, 'a>>,
}

impl<'c, 'a> Next<'c, 'a> {
    fn empty() -> Next<'c, 'a> {
        Next {
            ops: &[],
            outer: None,
        }
    }

    /// 先頭のアクセサと残りの連鎖。
    fn pop(self) -> Option<(&'a AccessorOp, Next<'c, 'a>)> {
        let mut cur = self;
        loop {
            if let Some((first, rest)) = cur.ops.split_first() {
                return Some((
                    first,
                    Next {
                        ops: rest,
                        outer: cur.outer,
                    },
                ));
            }
            cur = *cur.outer?;
        }
    }

    fn is_empty(self) -> bool {
        self.pop().is_none()
    }
}

/// `.*`・`[*]`・`.**`・配列展開の各項目へ適用するもの。
#[derive(Clone, Copy)]
enum Apply<'c, 'a> {
    /// 同じアクセサを各項目へ適用する（lax の配列展開）。
    Op(&'a AccessorOp, Next<'c, 'a>),
    /// 後続の連鎖を各項目へ適用する（空なら項目をそのまま結果へ）。
    Chain(Next<'c, 'a>),
}

struct Ev<'a> {
    root: &'a JsonValue,
    vars: Option<&'a BTreeMap<String, JsonValue>>,
    lax: bool,
    /// `false` のとき、抑止可能なエラーは例外にならず `Fail::Error` になる。
    throw: bool,
    ignore_structural: bool,
    current: Item<'a>,
    /// 添字式の中の `last` が参照する、最も内側の配列の長さ（添字の外では -1）。
    innermost: i64,
    steps_left: u64,
    /// 新規確保の残りバイト予算（複製・文字列生成の前に差し引く）。
    bytes_left: u64,
    depth: usize,
    max_depth: usize,
    /// `.keyvalue()` の `id` 用。コンテナのアドレス → 文書内の先行順番号（遅延構築）。
    id_map: Option<HashMap<usize, i64>>,
}

impl<'a> Ev<'a> {
    fn tick(&mut self, n: u64) -> R<()> {
        if n > self.steps_left {
            self.steps_left = 0;
            return Err(hard(JsonPathEvalError::StepLimitExceeded));
        }
        self.steps_left -= n;
        Ok(())
    }

    /// 確保の前にバイト予算を差し引く。超過は `54000`（抑止不可）。
    fn charge_bytes(&mut self, n: u64) -> R<()> {
        if n > self.bytes_left {
            self.bytes_left = 0;
            return Err(hard(JsonPathEvalError::ResultTooLarge));
        }
        self.bytes_left -= n;
        Ok(())
    }

    /// 数値テキスト（`Float` の `text`）を複製・生成する前にバイト予算を差し引く。
    fn charge_number(&mut self, n: &JsonNumber) -> R<()> {
        if let JsonNumber::Float { text, .. } = n {
            self.charge_bytes(u64::try_from(text.len()).unwrap_or(u64::MAX))?;
        }
        Ok(())
    }

    fn enter(&mut self) -> R<()> {
        self.tick(1)?;
        if self.depth >= self.max_depth {
            return Err(hard(JsonPathEvalError::DepthLimitExceeded));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// 抑止可能なエラー（PostgreSQL の `RETURN_ERROR`）。
    fn soft(&self, e: JsonPathEvalError) -> Fail {
        if self.throw {
            Fail::Thrown(e)
        } else {
            Fail::Error
        }
    }

    fn push(&mut self, found: &mut Vec<Item<'a>>, it: Item<'a>) -> R<()> {
        self.tick(1)?;
        if found.len() >= MAX_SEQUENCE_ITEMS {
            return Err(hard(JsonPathEvalError::ResultTooLarge));
        }
        found.push(it);
        Ok(())
    }

    /// 子項目を列挙する。確保の前に子の数だけ課金する（早期終了しても確保の大きさに
    /// 比例した課金になり、巨大な配列への繰り返し走査が有界時間で止まる）。
    fn charged_children(&mut self, item: &Item<'a>) -> R<Option<Vec<Item<'a>>>> {
        let Some(n) = item.child_count() else {
            return Ok(None);
        };
        self.tick(u64::try_from(n).unwrap_or(u64::MAX))?;
        Ok(item.children())
    }

    // --- 連鎖の実行 ---------------------------------------------------------

    /// `executeItem`: 項目を `lax` の自動展開つきで評価する。
    #[inline(always)]
    fn exec_item(&mut self, node: &'a PathNode, jb: &Item<'a>, found: Found<'_, 'a>) -> R<Res> {
        let unwrap = self.lax;
        self.exec_node(node, jb, Next::empty(), unwrap, found)
    }

    /// `executeNextItem`: 生成した項目 `v` を残りの連鎖へ渡す（連鎖が空なら結果へ追加）。
    fn exec_chain(
        &mut self,
        next: Next<'_, 'a>,
        v: Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        match next.pop() {
            Some((op, rest)) => self.exec_op(op, rest, &v, unwrap, found),
            None => match found {
                Some(f) => {
                    self.push(f, v)?;
                    Ok(Res::Ok)
                }
                None => Ok(Res::Ok),
            },
        }
    }

    fn apply(
        &mut self,
        ap: Apply<'_, 'a>,
        v: Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        match ap {
            Apply::Op(op, next) => self.exec_op(op, next, &v, unwrap, found),
            Apply::Chain(next) => self.exec_chain(next, v, unwrap, found),
        }
    }

    /// `executeAnyItem`: `jbc` の子孫（`first..=last` 階層）へ `ap` を適用する。JSON 側の
    /// 走査は明示スタックで行い、再帰しない。
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn exec_any(
        &mut self,
        ap: Apply<'_, 'a>,
        jbc: &Item<'a>,
        mut found: Found<'_, 'a>,
        first: u32,
        last: u32,
        ignore_structural: bool,
        unwrap_next: bool,
    ) -> R<Res> {
        let mut res = Res::NotFound;
        if 1 > last {
            return Ok(res);
        }
        let Some(children) = self.charged_children(jbc)? else {
            return Ok(res);
        };
        let mut stack: Vec<(u32, std::vec::IntoIter<Item<'a>>)> = Vec::new();
        stack.push((1, children.into_iter()));
        loop {
            let (level, v) = match stack.last_mut() {
                None => break,
                Some((level, it)) => match it.next() {
                    Some(v) => (*level, v),
                    None => {
                        stack.pop();
                        continue;
                    }
                },
            };
            let is_container = v.is_container();
            if level >= first || (first == u32::MAX && last == u32::MAX && !is_container) {
                let saved = self.ignore_structural;
                if ignore_structural {
                    self.ignore_structural = true;
                }
                let r = self.apply(ap, v.clone(), unwrap_next, found.as_deref_mut());
                self.ignore_structural = saved;
                res = r?;
                if res == Res::Ok && found.is_none() {
                    return Ok(Res::Ok);
                }
            }
            if level < last && is_container {
                if let Some(ch) = self.charged_children(&v)? {
                    let next_level = level.saturating_add(1);
                    stack.push((next_level, ch.into_iter()));
                }
            }
        }
        Ok(res)
    }

    /// `executeItemUnwrapTargetArray`: 配列の要素へ同じアクセサを適用する（1 段のみ）。
    fn unwrap_array(
        &mut self,
        op: &'a AccessorOp,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        self.exec_any(Apply::Op(op, next), jb, found, 1, 1, false, false)
    }

    // --- ノードの実行 -------------------------------------------------------

    fn exec_node(
        &mut self,
        node: &'a PathNode,
        jb: &Item<'a>,
        next: Next<'_, 'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        self.enter()?;
        let r = self.exec_node_inner(node, jb, next, unwrap, found);
        self.leave();
        r
    }

    /// ノード種別ごとの振り分けだけを行う。再帰の経路に載るため、局所変数の多い処理は
    /// `#[inline(never)]` の別関数へ出して、深い入れ子でもフレームを小さく保つ。
    #[inline(always)]
    fn exec_node_inner(
        &mut self,
        node: &'a PathNode,
        jb: &Item<'a>,
        next: Next<'_, 'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        match node {
            PathNode::Root | PathNode::Current | PathNode::Last | PathNode::Variable(_) => {
                self.exec_source(node, next, found)
            }
            PathNode::Null | PathNode::Bool(_) | PathNode::String(_) | PathNode::Numeric(_) => {
                self.exec_literal(node, next, found)
            }
            PathNode::Path { base, ops } => {
                let n = Next {
                    ops: ops.as_slice(),
                    outer: Some(&next),
                };
                self.exec_node(base, jb, n, unwrap, found)
            }
            PathNode::Unary { op, operand } => self.exec_unary(*op, operand, jb, next, found),
            PathNode::Arith { op, lhs, rhs } => self.exec_binary(*op, lhs, rhs, jb, next, found),
            _ => self.exec_bool_value(node, jb, next, found),
        }
    }

    /// `$`・`@`・`last`・`$name`。
    #[inline(never)]
    fn exec_source(
        &mut self,
        node: &'a PathNode,
        next: Next<'_, 'a>,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        let lax = self.lax;
        match node {
            PathNode::Root => self.exec_chain(next, Item::Ref(self.root), lax, found),
            PathNode::Current => {
                let cur = self.current.clone();
                self.exec_chain(next, cur, lax, found)
            }
            PathNode::Last => {
                if self.innermost < 0 {
                    // 添字外の `last` は PostgreSQL では構文段階で 42601 だが、本実装の
                    // 構文解析は受理するため、評価時に数値でない添字扱い（22033）で拒否する。
                    return Err(hard(JsonPathEvalError::SubscriptNotNumeric));
                }
                if next.is_empty() && found.is_none() {
                    return Ok(Res::Ok);
                }
                let last = self.innermost.saturating_sub(1);
                self.exec_chain(next, Item::Num(int_number(last)), lax, found)
            }
            PathNode::Variable(name) => {
                // 変数は結果を使わない存在判定でも評価する（未定義変数のエラーを出すため）。
                let v = self.lookup_var(name)?;
                self.exec_chain(next, Item::Ref(v), lax, found)
            }
            _ => Err(hard(JsonPathEvalError::Internal)),
        }
    }

    /// リテラル（`null`・真偽値・文字列・数値）。
    #[inline(never)]
    fn exec_literal(
        &mut self,
        node: &'a PathNode,
        next: Next<'_, 'a>,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        if next.is_empty() && found.is_none() {
            return Ok(Res::Ok);
        }
        let it = match node {
            PathNode::Null => Item::Null,
            PathNode::Bool(b) => Item::Bool(*b),
            PathNode::String(s) => Item::Str(Cow::Borrowed(&**s)),
            PathNode::Numeric(n) => Item::Num(literal_number(n)),
            _ => return Err(hard(JsonPathEvalError::Internal)),
        };
        let lax = self.lax;
        self.exec_chain(next, it, lax, found)
    }

    /// 述語を式として使った場合の値（`true`／`false`／`null`）。
    #[inline(never)]
    fn exec_bool_value(
        &mut self,
        node: &'a PathNode,
        jb: &Item<'a>,
        next: Next<'_, 'a>,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        let t = self.exec_bool(node, jb)?;
        if next.is_empty() && found.is_none() {
            return Ok(Res::Ok);
        }
        let it = match t {
            Tri::True => Item::Bool(true),
            Tri::False => Item::Bool(false),
            Tri::Unknown => Item::Null,
        };
        let lax = self.lax;
        self.exec_chain(next, it, lax, found)
    }

    fn lookup_var(&mut self, name: &str) -> R<&'a JsonValue> {
        self.vars
            .and_then(|m| m.get(name))
            .ok_or(hard(JsonPathEvalError::UndefinedVariable))
    }

    // --- アクセサ -----------------------------------------------------------

    fn exec_op(
        &mut self,
        op: &'a AccessorOp,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        self.enter()?;
        let r = self.exec_op_inner(op, next, jb, unwrap, found);
        self.leave();
        r
    }

    /// アクセサ種別の振り分けだけを行う（再帰の経路に載るためフレームを小さく保つ。
    /// 各アクセサの処理は `#[inline(never)]` の別関数）。
    #[inline(always)]
    fn exec_op_inner(
        &mut self,
        op: &'a AccessorOp,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        match op {
            AccessorOp::Member(key) => self.op_member(op, key, next, jb, unwrap, found),
            AccessorOp::MemberWildcard => self.op_member_wildcard(op, next, jb, unwrap, found),
            AccessorOp::ArrayWildcard => self.op_array_wildcard(next, jb, found),
            AccessorOp::Subscripts(subs) => self.op_subscripts(subs, next, jb, found),
            AccessorOp::AnyPath { from, to } => self.op_any_path(*from, *to, next, jb, found),
            AccessorOp::Filter(pred) => self.op_filter(op, pred, next, jb, unwrap, found),
            AccessorOp::Method(m) => self.exec_method(op, m, next, jb, unwrap, found),
        }
    }

    /// `.key`。
    #[inline(never)]
    fn op_member(
        &mut self,
        op: &'a AccessorOp,
        key: &str,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        if jb.kind() == Kind::Object {
            match jb.object_get(key) {
                Some(v) => {
                    let lax = self.lax;
                    self.exec_chain(next, v, lax, found)
                }
                None if !self.ignore_structural => {
                    Err(self.soft(JsonPathEvalError::MemberNotFound))
                }
                None => Ok(Res::NotFound),
            }
        } else if unwrap && jb.is_array() {
            self.unwrap_array(op, next, jb, found)
        } else if !self.ignore_structural {
            Err(self.soft(JsonPathEvalError::MemberNotFound))
        } else {
            Ok(Res::NotFound)
        }
    }

    /// `.*`。
    #[inline(never)]
    fn op_member_wildcard(
        &mut self,
        op: &'a AccessorOp,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        let lax = self.lax;
        if jb.kind() == Kind::Object {
            self.exec_any(Apply::Chain(next), jb, found, 1, 1, false, lax)
        } else if unwrap && jb.is_array() {
            self.unwrap_array(op, next, jb, found)
        } else if !self.ignore_structural {
            Err(self.soft(JsonPathEvalError::ObjectNotFound))
        } else {
            Ok(Res::NotFound)
        }
    }

    /// `[*]`。
    #[inline(never)]
    fn op_array_wildcard(
        &mut self,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        let lax = self.lax;
        if jb.is_array() {
            self.exec_any(Apply::Chain(next), jb, found, 1, 1, false, lax)
        } else if lax {
            self.exec_chain(next, jb.clone(), lax, found)
        } else if !self.ignore_structural {
            Err(self.soft(JsonPathEvalError::ArrayNotFound))
        } else {
            Ok(Res::NotFound)
        }
    }

    /// `[i, a to b, ...]`。
    #[inline(never)]
    fn op_subscripts(
        &mut self,
        subs: &'a [super::ast::Subscript],
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        let elems = jb.array_elems();
        if elems.is_some() || self.lax {
            let saved = self.innermost;
            let size = match elems {
                Some(e) => i64::try_from(e.len()).unwrap_or(i64::MAX),
                None => 1,
            };
            self.innermost = size;
            let r = self.exec_subscripts(subs, jb, elems, size, next, found);
            self.innermost = saved;
            r
        } else if !self.ignore_structural {
            Err(self.soft(JsonPathEvalError::ArrayNotFound))
        } else {
            Ok(Res::NotFound)
        }
    }

    /// `.**`・`.**{n to m}`。
    #[inline(never)]
    fn op_any_path(
        &mut self,
        from: AnyLevel,
        to: AnyLevel,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        mut found: Found<'_, 'a>,
    ) -> R<Res> {
        let lax = self.lax;
        let first = level_bound(from);
        let last = level_bound(to);
        let mut res = Res::NotFound;
        // PostgreSQL は 0 階層目の評価結果が抑止されたエラー（`jperError`）でも、子孫の走査が
        // あればその結果で上書きする（走査できない値のときだけエラーが残る）。
        let mut suppressed = false;
        if first == 0 {
            let saved = self.ignore_structural;
            self.ignore_structural = true;
            let r = self.exec_chain(next, jb.clone(), lax, found.as_deref_mut());
            self.ignore_structural = saved;
            match r {
                Ok(x) => res = x,
                Err(Fail::Error) => suppressed = true,
                Err(e) => return Err(e),
            }
            if res == Res::Ok && found.is_none() {
                return Ok(Res::Ok);
            }
        }
        if jb.is_container() {
            res = self.exec_any(Apply::Chain(next), jb, found, first, last, true, lax)?;
        } else if suppressed {
            return Err(Fail::Error);
        }
        Ok(res)
    }

    /// `? (述語)`。
    #[inline(never)]
    fn op_filter(
        &mut self,
        op: &'a AccessorOp,
        pred: &'a PathNode,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        if unwrap && jb.is_array() {
            return self.unwrap_array(op, next, jb, found);
        }
        let st = self.exec_nested_bool(pred, jb)?;
        if st != Tri::True {
            Ok(Res::NotFound)
        } else {
            let lax = self.lax;
            self.exec_chain(next, jb.clone(), lax, found)
        }
    }

    #[inline(never)]
    fn exec_subscripts(
        &mut self,
        subs: &'a [super::ast::Subscript],
        jb: &Item<'a>,
        elems: Option<&'a [JsonValue]>,
        size: i64,
        next: Next<'_, 'a>,
        mut found: Found<'_, 'a>,
    ) -> R<Res> {
        let lax = self.lax;
        let has_next = !next.is_empty();
        let mut res = Res::NotFound;
        for sub in subs {
            let index_from = self.get_array_index(&sub.from, jb)?;
            let index_to = match &sub.to {
                Some(to) => self.get_array_index(to, jb)?,
                None => index_from,
            };
            if !self.ignore_structural
                && (index_from < 0 || index_from > index_to || index_to >= size)
            {
                return Err(self.soft(JsonPathEvalError::SubscriptOutOfBounds));
            }
            let from = index_from.max(0);
            let to = index_to.min(size.saturating_sub(1));
            res = Res::NotFound;
            let mut index = from;
            while index <= to {
                self.tick(1)?;
                let v = match elems {
                    None => jb.clone(),
                    Some(e) => match usize::try_from(index).ok().and_then(|i| e.get(i)) {
                        Some(x) => Item::Ref(x),
                        None => {
                            index += 1;
                            continue;
                        }
                    },
                };
                if !has_next && found.is_none() {
                    return Ok(Res::Ok);
                }
                res = self.exec_chain(next, v, lax, found.as_deref_mut())?;
                if res == Res::Ok && found.is_none() {
                    break;
                }
                index += 1;
            }
            if res == Res::Ok && found.is_none() {
                break;
            }
        }
        Ok(res)
    }

    /// 添字式を評価して int4 の添字（小数部は切り捨て）にする。
    #[inline(never)]
    fn get_array_index(&mut self, node: &'a PathNode, jb: &Item<'a>) -> R<i64> {
        let mut seq: Vec<Item<'a>> = Vec::new();
        // PostgreSQL 17 の実測（TABLE-25 fixture）では lax でも結果配列はアンラップされない。
        self.exec_item(node, jb, Some(&mut seq))?;
        let n = match seq.as_slice() {
            [one] => one.num(),
            _ => None,
        };
        let Some(n) = n else {
            return Err(self.soft(JsonPathEvalError::SubscriptNotNumeric));
        };
        let idx = num::from_json_number(n)
            .and_then(|d| num::trunc_to_i128(&d))
            .and_then(|t| i32::try_from(t).ok());
        match idx {
            Some(i) => Ok(i64::from(i)),
            None => Err(self.soft(JsonPathEvalError::SubscriptOutOfRange)),
        }
    }

    // --- 項目メソッド -------------------------------------------------------

    /// 項目メソッドの振り分け。再帰の経路に載る `.type()`／`.size()` は小さな専用関数へ、
    /// 値を作るメソッドは `method_value`（再帰しない部分を `apply_value_method` に分離）へ。
    fn exec_method(
        &mut self,
        op: &'a AccessorOp,
        m: &'a Method,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        match m {
            Method::Type | Method::Size => self.method_type_size(m, next, jb, found),
            Method::Date
            | Method::Datetime(_)
            | Method::Time(_)
            | Method::TimeTz(_)
            | Method::Timestamp(_)
            | Method::TimestampTz(_) => Err(hard(JsonPathEvalError::Unsupported)),
            _ => self.method_value(op, m, next, jb, unwrap, found),
        }
    }

    /// `.type()`・`.size()`（配列の自動展開をしないメソッド）。
    #[inline(never)]
    fn method_type_size(
        &mut self,
        m: &'a Method,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        let lax = self.lax;
        if let Method::Type = m {
            let it = Item::Str(Cow::Borrowed(jb.type_name()));
            return self.exec_chain(next, it, lax, found);
        }
        let size = match jb.array_elems() {
            Some(e) => i64::try_from(e.len()).unwrap_or(i64::MAX),
            None => {
                if !lax {
                    if !self.ignore_structural {
                        return Err(self.soft(JsonPathEvalError::ArrayNotFound));
                    }
                    return Ok(Res::NotFound);
                }
                1
            }
        };
        self.exec_chain(next, Item::Num(int_number(size)), lax, found)
    }

    /// 値を 1 つ作るメソッドと `.keyvalue()`（配列は lax で要素へ展開する）。
    #[inline(never)]
    fn method_value(
        &mut self,
        op: &'a AccessorOp,
        m: &'a Method,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        if unwrap && jb.is_array() {
            return self.unwrap_array(op, next, jb, found);
        }
        if let Method::KeyValue = m {
            return self.exec_keyvalue(next, jb, found);
        }
        let out = self.apply_value_method(m, jb)?;
        let lax = self.lax;
        self.exec_chain(next, out, lax, found)
    }

    #[inline(never)]
    fn exec_keyvalue(
        &mut self,
        next: Next<'_, 'a>,
        jb: &Item<'a>,
        mut found: Found<'_, 'a>,
    ) -> R<Res> {
        let lax = self.lax;
        let pairs: Vec<(&'a str, &'a JsonValue)> = match jb {
            // `.keyvalue()` の結果への再適用は未対応（黙って空にせず fail-closed にする）。
            Item::KeyValue { .. } => return Err(hard(JsonPathEvalError::Unsupported)),
            Item::Ref(JsonValue::Object(m)) => {
                self.tick(u64::try_from(m.len()).unwrap_or(u64::MAX))?;
                let mut p: Vec<(&'a str, &'a JsonValue)> =
                    m.iter().map(|(k, v)| (k.as_str(), v)).collect();
                p.sort_by(|a, b| pg_key_order(a.0, b.0));
                p
            }
            _ => return Err(self.soft(JsonPathEvalError::ObjectNotFound)),
        };
        if pairs.is_empty() {
            return Ok(Res::NotFound);
        }
        let id = match jb {
            Item::Ref(obj) => self.object_id(obj)?,
            _ => 0,
        };
        let mut res = Res::NotFound;
        for (key, value) in pairs {
            // 新たに作るオブジェクトの深さ（値の深さ + 1）が上限を超えないこと。
            let d = self.depth_of(value)?;
            if d >= MAX_JSON_DEPTH {
                return Err(hard(JsonPathEvalError::ResultTooLarge));
            }
            let it = Item::KeyValue { id, key, value };
            res = self.exec_chain(next, it, lax, found.as_deref_mut())?;
            if res == Res::Ok && found.is_none() {
                break;
            }
        }
        Ok(res)
    }

    /// コンテナの深さ（スカラーは 0）。明示スタックで数え、訪問ノードごとに課金する。
    fn depth_of(&mut self, v: &'a JsonValue) -> R<usize> {
        let mut max = 0usize;
        let mut stack: Vec<(&'a JsonValue, usize)> = vec![(v, 0)];
        while let Some((x, d)) = stack.pop() {
            self.tick(1)?;
            match x {
                JsonValue::Array(a) => {
                    let nd = d.saturating_add(1);
                    max = max.max(nd);
                    for c in a {
                        stack.push((c, nd));
                    }
                }
                JsonValue::Object(m) => {
                    let nd = d.saturating_add(1);
                    max = max.max(nd);
                    for c in m.values() {
                        stack.push((c, nd));
                    }
                }
                _ => {}
            }
        }
        Ok(max)
    }

    /// `.keyvalue()` の `id`: ルートのオブジェクトは 0（PostgreSQL と一致）、それ以外は
    /// 文書内の先行順番号（PostgreSQL のバイトオフセット由来の値は再現しない。意図的な差分）。
    fn object_id(&mut self, obj: &'a JsonValue) -> R<i64> {
        if std::ptr::eq(obj, self.root) {
            return Ok(0);
        }
        if self.id_map.is_none() {
            let mut map: HashMap<usize, i64> = HashMap::new();
            let mut counter: i64 = 0;
            let mut stack: Vec<&'a JsonValue> = vec![self.root];
            while let Some(x) = stack.pop() {
                self.tick(1)?;
                let addr = x as *const JsonValue as usize;
                match x {
                    JsonValue::Array(a) => {
                        map.insert(addr, counter);
                        counter = counter.saturating_add(1);
                        for c in a.iter().rev() {
                            stack.push(c);
                        }
                    }
                    JsonValue::Object(m) => {
                        map.insert(addr, counter);
                        counter = counter.saturating_add(1);
                        let mut vals: Vec<(&String, &'a JsonValue)> = m.iter().collect();
                        vals.sort_by(|a, b| pg_key_order(a.0, b.0));
                        for (_, c) in vals.into_iter().rev() {
                            stack.push(c);
                        }
                    }
                    _ => {}
                }
            }
            self.id_map = Some(map);
        }
        let addr = obj as *const JsonValue as usize;
        Ok(self
            .id_map
            .as_ref()
            .and_then(|m| m.get(&addr))
            .copied()
            .unwrap_or(0))
    }

    /// 値を 1 つ返す項目メソッド（配列展開・`.keyvalue()` 以外）。
    #[inline(never)]
    fn apply_value_method(&mut self, m: &'a Method, jb: &Item<'a>) -> R<Item<'a>> {
        match m {
            Method::Abs | Method::Floor | Method::Ceiling => {
                let Some(n) = jb.num() else {
                    return Err(self.soft(JsonPathEvalError::NonNumericItem));
                };
                self.numeric_method(m, n)
            }
            Method::Double => {
                // 数値入力は（有限であることを確かめたうえで）同じ値を返し、文字列入力だけが
                // `float8` を経由する（PostgreSQL 17 と同じ）。
                match jb.kind() {
                    Kind::Num => match jb.num() {
                        Some(n) if n.as_f64().is_finite() && !underflows(n) => {
                            Ok(Item::Num(n.clone()))
                        }
                        _ => Err(self.soft(JsonPathEvalError::NonNumericItem)),
                    },
                    Kind::Str => match jb.str().and_then(parse_float8_text) {
                        Some(f) => Ok(Item::Num(float8_to_number(f))),
                        None => Err(self.soft(JsonPathEvalError::NonNumericItem)),
                    },
                    _ => Err(self.soft(JsonPathEvalError::NonNumericItem)),
                }
            }
            Method::BigInt => self.int_method(jb, i128::from(i64::MIN), i128::from(i64::MAX)),
            Method::Integer => self.int_method(jb, i128::from(i32::MIN), i128::from(i32::MAX)),
            Method::Number => match jb.kind() {
                Kind::Num => {
                    let n = jb.num().ok_or(hard(JsonPathEvalError::Internal))?;
                    self.charge_number(n)?;
                    Ok(Item::Num(n.clone()))
                }
                Kind::Str => match jb.str().and_then(parse_numeric_text) {
                    Some(n) => {
                        self.charge_number(&n)?;
                        Ok(Item::Num(n))
                    }
                    None => Err(self.soft(JsonPathEvalError::NonNumericItem)),
                },
                _ => Err(self.soft(JsonPathEvalError::NonNumericItem)),
            },
            Method::Decimal { precision, scale } => {
                let n: JsonNumber = match jb.kind() {
                    Kind::Num => {
                        let n = jb.num().ok_or(hard(JsonPathEvalError::Internal))?;
                        self.charge_number(n)?;
                        n.clone()
                    }
                    Kind::Str => match jb.str().and_then(parse_numeric_text) {
                        Some(n) => {
                            self.charge_number(&n)?;
                            n
                        }
                        None => return Err(self.soft(JsonPathEvalError::NonNumericItem)),
                    },
                    _ => return Err(self.soft(JsonPathEvalError::NonNumericItem)),
                };
                let args = self.decimal_args(precision.as_deref(), scale.as_deref())?;
                match args {
                    None => Ok(Item::Num(n)),
                    Some((p, s)) => {
                        let Some(d) = num::from_json_number(&n) else {
                            return Err(self.soft(JsonPathEvalError::NonNumericItem));
                        };
                        match num::apply_precision_scale(&d, p, s) {
                            Ok(r) => Ok(decimal_number(r)),
                            Err(_) => Err(self.soft(JsonPathEvalError::NonNumericItem)),
                        }
                    }
                }
            }
            Method::Boolean => match jb.kind() {
                Kind::Bool => match jb {
                    Item::Bool(b) => Ok(Item::Bool(*b)),
                    Item::Ref(JsonValue::Bool(b)) => Ok(Item::Bool(*b)),
                    _ => Err(hard(JsonPathEvalError::Internal)),
                },
                Kind::Num => {
                    // PG 17 実測表（jsonpath_pg17_table.inc）に合わせ、scale 付き（2.0・0.0 等）は
                    // 拒否し、scale 0 で int4 に収まる整数だけ受理する（拒否は 22036）。
                    let v = jb
                        .num()
                        .and_then(num::from_json_number)
                        .filter(|d| d.scale() == 0)
                        .and_then(|d| num::round_to_i128(&d))
                        .and_then(|t| i32::try_from(t).ok());
                    match v {
                        Some(v) => Ok(Item::Bool(v != 0)),
                        None => Err(self.soft(JsonPathEvalError::NonNumericItem)),
                    }
                }
                Kind::Str => match jb.str().and_then(parse_bool_text) {
                    Some(b) => Ok(Item::Bool(b)),
                    None => Err(self.soft(JsonPathEvalError::NonNumericItem)),
                },
                _ => Err(self.soft(JsonPathEvalError::NonNumericItem)),
            },
            Method::String => match jb.kind() {
                Kind::Str => {
                    let src = jb.str().unwrap_or("");
                    self.charge_bytes(u64::try_from(src.len()).unwrap_or(u64::MAX))?;
                    Ok(Item::Str(Cow::Owned(src.to_string())))
                }
                Kind::Bool => {
                    let b = matches!(jb, Item::Bool(true) | Item::Ref(JsonValue::Bool(true)));
                    Ok(Item::Str(Cow::Borrowed(if b { "true" } else { "false" })))
                }
                Kind::Num => {
                    let text = jb.num().map(number_text).unwrap_or_default();
                    Ok(Item::Str(Cow::Owned(text)))
                }
                _ => Err(self.soft(JsonPathEvalError::NonNumericItem)),
            },
            _ => Err(hard(JsonPathEvalError::Internal)),
        }
    }

    #[inline(never)]
    fn numeric_method(&mut self, m: &Method, n: &JsonNumber) -> R<Item<'a>> {
        if let Method::Abs = m {
            // 巨大な値でも壊れないよう、符号の除去は桁文字列のまま行う。
            return Ok(Item::Num(match n {
                JsonNumber::PosInt(v) => JsonNumber::PosInt(*v),
                JsonNumber::NegInt(v) => JsonNumber::PosInt(v.unsigned_abs()),
                JsonNumber::Float { value, text } => JsonNumber::Float {
                    value: value.abs(),
                    text: text.trim_start_matches('-').into(),
                },
            }));
        }
        let Some(d) = num::from_json_number(n) else {
            return Err(self.soft(JsonPathEvalError::NumericOutOfRange));
        };
        let r = match m {
            Method::Floor => num::floor(&d),
            _ => num::ceil(&d),
        };
        match r {
            Ok(v) => Ok(decimal_number(v)),
            Err(_) => Err(self.soft(JsonPathEvalError::NumericOutOfRange)),
        }
    }

    /// `.bigint()`／`.integer()`: 数値は 0 から遠い側へ丸め、文字列は整数リテラルとして読む。
    #[inline(never)]
    fn int_method(&mut self, jb: &Item<'a>, lo: i128, hi: i128) -> R<Item<'a>> {
        let v: Option<i128> = match jb.kind() {
            Kind::Num => jb
                .num()
                .and_then(num::from_json_number)
                .and_then(|d| num::round_to_i128(&d)),
            Kind::Str => jb.str().and_then(parse_int_text),
            _ => None,
        };
        match v.filter(|v| (lo..=hi).contains(v)) {
            Some(v) => match i64::try_from(v) {
                Ok(i) => Ok(Item::Num(int_number(i))),
                Err(_) => Err(self.soft(JsonPathEvalError::NonNumericItem)),
            },
            None => Err(self.soft(JsonPathEvalError::NonNumericItem)),
        }
    }

    /// `.decimal(p, s)` の引数検査。引数なしは `None`。p／s の範囲違反は抑止可能な `22023`。
    #[inline(never)]
    fn decimal_args(&mut self, p: Option<&str>, s: Option<&str>) -> R<Option<(i64, i64)>> {
        let Some(p) = p else {
            return Ok(None);
        };
        let parse = |t: &str| -> Option<i128> { t.parse::<i128>().ok() };
        let Some(pv) = parse(p) else {
            return Err(self.soft(JsonPathEvalError::NonNumericItem));
        };
        let sv = match s {
            Some(t) => match parse(t) {
                Some(v) => v,
                None => return Err(self.soft(JsonPathEvalError::NonNumericItem)),
            },
            None => 0,
        };
        // int4 の範囲外は「項目が数値でない」扱い、範囲内で 1..=1000／-1000..=1000 の外は
        // 引数不正（PostgreSQL 17 の挙動）。
        let int4 = i128::from(i32::MIN)..=i128::from(i32::MAX);
        if !int4.contains(&pv) || !int4.contains(&sv) {
            return Err(self.soft(JsonPathEvalError::NonNumericItem));
        }
        if !(1..=1000).contains(&pv) || !(-1000..=1000).contains(&sv) {
            return Err(self.soft(JsonPathEvalError::InvalidMethodArgument));
        }
        let pv = i64::try_from(pv).map_err(|_| hard(JsonPathEvalError::Internal))?;
        let sv = i64::try_from(sv).map_err(|_| hard(JsonPathEvalError::Internal))?;
        Ok(Some((pv, sv)))
    }

    // --- 算術 ---------------------------------------------------------------

    #[inline(never)]
    fn exec_unary(
        &mut self,
        op: UnaryOp,
        operand: &'a PathNode,
        jb: &Item<'a>,
        next: Next<'_, 'a>,
        mut found: Found<'_, 'a>,
    ) -> R<Res> {
        let lax = self.lax;
        let mut seq: Vec<Item<'a>> = Vec::new();
        self.exec_opt_unwrap_result(operand, jb, true, Some(&mut seq))?;
        let has_next = !next.is_empty();
        let mut res = Res::NotFound;
        for val in seq {
            self.tick(1)?;
            let Some(n) = val.num() else {
                if found.is_none() && !has_next {
                    continue;
                }
                return Err(self.soft(JsonPathEvalError::NumberRequired));
            };
            if found.is_none() && !has_next {
                return Ok(Res::Ok);
            }
            let out = match op {
                UnaryOp::Plus => n.clone(),
                UnaryOp::Minus => match num::from_json_number(n).and_then(|d| num::neg(&d).ok()) {
                    Some(d) => num::to_json_number(d),
                    // 表現範囲外の巨大な値は、綴りの符号だけを反転する。
                    None => negate_text(n),
                },
            };
            let r = self.exec_chain(next, Item::Num(out), lax, found.as_deref_mut())?;
            if r == Res::Ok {
                if found.is_none() {
                    return Ok(Res::Ok);
                }
                res = Res::Ok;
            }
        }
        Ok(res)
    }

    /// 二項算術。左結合の連鎖（`1 + 1 + 1 + ...`）は式木の高さに比例して深くなるため、左の背骨を
    /// 再帰せず反復で畳む（最も内側から順に評価するので、エラーの発生順は再帰と同じ）。
    #[inline(never)]
    fn exec_binary(
        &mut self,
        op: ArithOp,
        lhs: &'a PathNode,
        rhs: &'a PathNode,
        jb: &Item<'a>,
        next: Next<'_, 'a>,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        // 外側 → 内側の順に、各ノードの演算子と右辺を集める。
        let mut spine: Vec<(ArithOp, &'a PathNode)> = vec![(op, rhs)];
        let mut leftmost = lhs;
        while let PathNode::Arith { op, lhs, rhs } = leftmost {
            spine.push((*op, rhs));
            leftmost = lhs;
        }
        let mut cur = self.operand_number(leftmost, jb)?;
        // 最外殻（先頭要素）以外は、結果の数値をそのまま次の左辺に使う。
        while spine.len() > 1 {
            let Some((o, r)) = spine.pop() else { break };
            self.tick(1)?;
            let rv = self.operand_number(r, jb)?;
            cur = Some(self.combine_numbers(o, cur, rv)?);
        }
        let Some((o, r)) = spine.pop() else {
            return Err(hard(JsonPathEvalError::Internal));
        };
        self.tick(1)?;
        let rv = self.operand_number(r, jb)?;
        let out = self.combine_numbers(o, cur, rv)?;
        let lax = self.lax;
        self.exec_chain(next, Item::Num(out), lax, found)
    }

    /// 算術オペランドを評価し、単一の数値ならそれを返す（それ以外は `None`）。
    #[inline(never)]
    fn operand_number(&mut self, node: &'a PathNode, jb: &Item<'a>) -> R<Option<JsonNumber>> {
        let mut seq: Vec<Item<'a>> = Vec::new();
        self.exec_opt_unwrap_result(node, jb, true, Some(&mut seq))?;
        Ok(match seq.as_slice() {
            [one] => one.num().cloned(),
            _ => None,
        })
    }

    /// 両オペランドが評価済みの二項演算（再帰しない）。
    #[inline(never)]
    fn combine_numbers(
        &mut self,
        op: ArithOp,
        l: Option<JsonNumber>,
        r: Option<JsonNumber>,
    ) -> R<JsonNumber> {
        let Some(l) = l else {
            return Err(self.soft(JsonPathEvalError::SingletonRequired));
        };
        let Some(r) = r else {
            return Err(self.soft(JsonPathEvalError::SingletonRequired));
        };
        let (Some(a), Some(b)) = (num::from_json_number(&l), num::from_json_number(&r)) else {
            return Err(self.soft(JsonPathEvalError::NumericOutOfRange));
        };
        let out = match op {
            ArithOp::Add => num::add(&a, &b),
            ArithOp::Sub => num::sub(&a, &b),
            ArithOp::Mul => num::mul(&a, &b),
            ArithOp::Div => num::div(&a, &b),
            ArithOp::Mod => num::rem(&a, &b),
        };
        match out {
            Ok(d) => Ok(num::to_json_number(d)),
            Err(num::NumFail::DivisionByZero) => Err(self.soft(JsonPathEvalError::DivisionByZero)),
            Err(num::NumFail::Overflow) => Err(self.soft(JsonPathEvalError::NumericOutOfRange)),
        }
    }

    // --- 述語 ---------------------------------------------------------------

    /// `executeItemOptUnwrapResult`: lax で `unwrap` なら結果の配列を 1 段展開する。
    #[inline(always)]
    fn exec_opt_unwrap_result(
        &mut self,
        node: &'a PathNode,
        jb: &Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        if unwrap && self.lax {
            self.exec_unwrapped(node, jb, found)
        } else {
            self.exec_item(node, jb, found)
        }
    }

    /// lax の自動展開つきで評価し、結果の配列を 1 段展開して `found` へ渡す。
    #[inline(never)]
    fn exec_unwrapped(
        &mut self,
        node: &'a PathNode,
        jb: &Item<'a>,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        let mut seq: Vec<Item<'a>> = Vec::new();
        self.exec_item(node, jb, Some(&mut seq))?;
        let Some(out) = found else {
            return Ok(Res::Ok);
        };
        for item in seq {
            if item.is_array() {
                if let Some(ch) = self.charged_children(&item)? {
                    for c in ch {
                        self.push(out, c)?;
                    }
                }
            } else {
                self.push(out, item)?;
            }
        }
        Ok(Res::Ok)
    }

    /// `executeItemOptUnwrapResultNoThrow`: 抑止可能なエラーを例外にしない。
    fn exec_opt_unwrap_result_nothrow(
        &mut self,
        node: &'a PathNode,
        jb: &Item<'a>,
        unwrap: bool,
        found: Found<'_, 'a>,
    ) -> R<Res> {
        let saved = self.throw;
        self.throw = false;
        let r = self.exec_opt_unwrap_result(node, jb, unwrap, found);
        self.throw = saved;
        r
    }

    /// オペランド列を評価する。抑止されたエラーは `None`（呼び出し側で unknown）。
    fn eval_operand(
        &mut self,
        node: &'a PathNode,
        jb: &Item<'a>,
        unwrap: bool,
    ) -> R<Option<Vec<Item<'a>>>> {
        let mut seq: Vec<Item<'a>> = Vec::new();
        match self.exec_opt_unwrap_result_nothrow(node, jb, unwrap, Some(&mut seq)) {
            Ok(_) => Ok(Some(seq)),
            Err(Fail::Error) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `executeNestedBoolItem`: フィルタ内の述語。`@` を差し替える（lax の構造エラー抑止は
    /// フィルタ内でも有効）。
    fn exec_nested_bool(&mut self, pred: &'a PathNode, jb: &Item<'a>) -> R<Tri> {
        let saved_current = std::mem::replace(&mut self.current, jb.clone());
        let r = self.exec_bool(pred, jb);
        self.current = saved_current;
        r
    }

    fn exec_bool(&mut self, node: &'a PathNode, jb: &Item<'a>) -> R<Tri> {
        self.enter()?;
        let r = self.exec_bool_inner(node, jb);
        self.leave();
        r
    }

    #[inline(always)]
    fn exec_bool_inner(&mut self, node: &'a PathNode, jb: &Item<'a>) -> R<Tri> {
        match node {
            PathNode::And(l, r) => {
                let a = self.exec_bool(l, jb)?;
                if a == Tri::False {
                    return Ok(Tri::False);
                }
                let b = self.exec_bool(r, jb)?;
                Ok(if b == Tri::True { a } else { b })
            }
            PathNode::Or(l, r) => {
                let a = self.exec_bool(l, jb)?;
                if a == Tri::True {
                    return Ok(Tri::True);
                }
                let b = self.exec_bool(r, jb)?;
                Ok(if b == Tri::False { a } else { b })
            }
            PathNode::Not(a) => Ok(match self.exec_bool(a, jb)? {
                Tri::Unknown => Tri::Unknown,
                Tri::True => Tri::False,
                Tri::False => Tri::True,
            }),
            PathNode::IsUnknown(a) => Ok(if self.exec_bool(a, jb)? == Tri::Unknown {
                Tri::True
            } else {
                Tri::False
            }),
            PathNode::Compare { op, lhs, rhs } => self.bool_compare(*op, lhs, rhs, jb),
            PathNode::StartsWith { lhs, prefix } => self.bool_starts_with(lhs, prefix, jb),
            PathNode::Exists(arg) => self.bool_exists(arg, jb),
            PathNode::LikeRegex { .. } => Err(hard(JsonPathEvalError::Unsupported)),
            _ => Err(hard(JsonPathEvalError::Internal)),
        }
    }

    /// 比較述語（`==`・`<` 等）。
    #[inline(never)]
    fn bool_compare(
        &mut self,
        op: CompareOp,
        lhs: &'a PathNode,
        rhs: &'a PathNode,
        jb: &Item<'a>,
    ) -> R<Tri> {
        let Some(lseq) = self.eval_operand(lhs, jb, true)? else {
            return Ok(Tri::Unknown);
        };
        let Some(rseq) = self.eval_operand(rhs, jb, true)? else {
            return Ok(Tri::Unknown);
        };
        self.pair_loop(&lseq, &rseq, |l, r| compare_items(op, l, r))
    }

    /// `starts with`。
    #[inline(never)]
    fn bool_starts_with(
        &mut self,
        lhs: &'a PathNode,
        prefix: &'a StartsWithOperand,
        jb: &Item<'a>,
    ) -> R<Tri> {
        let Some(lseq) = self.eval_operand(lhs, jb, true)? else {
            return Ok(Tri::Unknown);
        };
        let rseq: Vec<Item<'a>> = match prefix {
            StartsWithOperand::String(s) => vec![Item::Str(Cow::Borrowed(&**s))],
            StartsWithOperand::Variable(name) => {
                vec![Item::Ref(self.lookup_var(name)?)]
            }
        };
        self.pair_loop(&lseq, &rseq, starts_with_items)
    }

    /// `exists(...)`。
    #[inline(never)]
    fn bool_exists(&mut self, arg: &'a PathNode, jb: &Item<'a>) -> R<Tri> {
        if !self.lax {
            let mut vals: Vec<Item<'a>> = Vec::new();
            match self.exec_opt_unwrap_result_nothrow(arg, jb, false, Some(&mut vals)) {
                Ok(_) => Ok(if vals.is_empty() {
                    Tri::False
                } else {
                    Tri::True
                }),
                Err(Fail::Error) => Ok(Tri::Unknown),
                Err(e) => Err(e),
            }
        } else {
            match self.exec_opt_unwrap_result_nothrow(arg, jb, false, None) {
                Ok(Res::Ok) => Ok(Tri::True),
                Ok(Res::NotFound) => Ok(Tri::False),
                Err(Fail::Error) => Ok(Tri::Unknown),
                Err(e) => Err(e),
            }
        }
    }

    /// `executePredicate` の組の走査。strict は最初の unknown で unknown、lax は最初の
    /// true で true。
    #[inline(never)]
    fn pair_loop(
        &mut self,
        lseq: &[Item<'a>],
        rseq: &[Item<'a>],
        f: impl Fn(&Item<'a>, &Item<'a>) -> Tri,
    ) -> R<Tri> {
        let strict = !self.lax;
        let mut error = false;
        let mut hit = false;
        for l in lseq {
            for r in rseq {
                self.tick(1)?;
                // 文字列対は走査バイト数に比例して追加課金する（fail-closed）。
                self.tick(pair_scan_steps(l, r))?;
                match f(l, r) {
                    Tri::Unknown => {
                        if strict {
                            return Ok(Tri::Unknown);
                        }
                        error = true;
                    }
                    Tri::True => {
                        if !strict {
                            return Ok(Tri::True);
                        }
                        hit = true;
                    }
                    Tri::False => {}
                }
            }
        }
        Ok(if hit {
            Tri::True
        } else if error {
            Tri::Unknown
        } else {
            Tri::False
        })
    }
}

/// 数値の綴りが 0 以外の仮数を持つのに `f64` で 0 になる（アンダーフロー）か。
fn underflows(n: &JsonNumber) -> bool {
    match n {
        JsonNumber::Float { value, text } => {
            *value == 0.0
                && text
                    .bytes()
                    .take_while(|b| !matches!(b, b'e' | b'E'))
                    .any(|b| (b'1'..=b'9').contains(&b))
        }
        _ => false,
    }
}

/// 符号の反転（`Decimal` に収まらない値向けに、綴りの先頭の `-` を付け外しする）。
fn negate_text(n: &JsonNumber) -> JsonNumber {
    match n {
        JsonNumber::PosInt(0) => JsonNumber::PosInt(0),
        JsonNumber::PosInt(v) => match i64::try_from(*v) {
            Ok(i) => JsonNumber::NegInt(-i),
            Err(_) => JsonNumber::Float {
                value: -(*v as f64),
                text: format!("-{v}").into_boxed_str(),
            },
        },
        JsonNumber::NegInt(v) => match v.checked_neg() {
            Some(p) => int_number(p),
            None => JsonNumber::PosInt(v.unsigned_abs()),
        },
        JsonNumber::Float { value, text } => {
            let t = match text.strip_prefix('-') {
                Some(r) => r.to_string(),
                None => format!("-{text}"),
            };
            JsonNumber::Float {
                value: -*value,
                text: t.into_boxed_str(),
            }
        }
    }
}

fn level_bound(l: AnyLevel) -> u32 {
    match l {
        AnyLevel::Level(n) => n,
        AnyLevel::Last => u32::MAX,
    }
}

// ---------------------------------------------------------------------------
// 比較・文字列変換
// ---------------------------------------------------------------------------

/// 文字列比較 1 組あたりの追加課金の粒度（バイト）。
const STR_SCAN_BYTES_PER_STEP: usize = 64;

/// 文字列どうしの比較・前方一致で走査しうるバイト数（短い側の長さ）に比例した追加ステップ数。
/// `pair_loop` から呼ばれ、巨大文字列の大量比較による処理量の爆発を上限エラーに倒す。
fn pair_scan_steps(l: &Item<'_>, r: &Item<'_>) -> u64 {
    match (l.str(), r.str()) {
        (Some(a), Some(b)) => {
            u64::try_from(a.len().min(b.len()) / STR_SCAN_BYTES_PER_STEP).unwrap_or(u64::MAX)
        }
        _ => 0,
    }
}

/// 比較の型クラス。数値・文字列・真偽値・null は同型どうしだけ比較でき、コンテナは
/// 比較不能（unknown）。
fn compare_items(op: CompareOp, l: &Item<'_>, r: &Item<'_>) -> Tri {
    let (lk, rk) = (l.kind(), r.kind());
    let class = |k: Kind| match k {
        Kind::Array | Kind::Object => Kind::Array,
        other => other,
    };
    if class(lk) != class(rk) {
        if lk == Kind::Null || rk == Kind::Null {
            // null と非 null の等価・順序比較は常に false、不等比較だけ true。
            return if op == CompareOp::Ne {
                Tri::True
            } else {
                Tri::False
            };
        }
        return Tri::Unknown;
    }
    let ord: Ordering = match lk {
        Kind::Null => Ordering::Equal,
        Kind::Bool => {
            let b = |i: &Item<'_>| matches!(i, Item::Bool(true) | Item::Ref(JsonValue::Bool(true)));
            b(l).cmp(&b(r))
        }
        Kind::Num => match (l.num(), r.num()) {
            (Some(a), Some(b)) => match cmp_json_numbers(a, b) {
                Some(o) => o,
                None => return Tri::Unknown,
            },
            _ => return Tri::Unknown,
        },
        Kind::Str => match (l.str(), r.str()) {
            (Some(a), Some(b)) => a.as_bytes().cmp(b.as_bytes()),
            _ => return Tri::Unknown,
        },
        Kind::Array | Kind::Object => return Tri::Unknown,
    };
    let t = match op {
        CompareOp::Eq => ord == Ordering::Equal,
        CompareOp::Ne => ord != Ordering::Equal,
        CompareOp::Lt => ord == Ordering::Less,
        CompareOp::Le => ord != Ordering::Greater,
        CompareOp::Gt => ord == Ordering::Greater,
        CompareOp::Ge => ord != Ordering::Less,
    };
    if t {
        Tri::True
    } else {
        Tri::False
    }
}

fn starts_with_items(whole: &Item<'_>, initial: &Item<'_>) -> Tri {
    match (whole.str(), initial.str()) {
        (Some(w), Some(i)) => {
            if w.as_bytes().starts_with(i.as_bytes()) {
                Tri::True
            } else {
                Tri::False
            }
        }
        _ => Tri::Unknown,
    }
}

/// 数値の文字列化（`.string()`）。`i128` に収まれば正規の 10 進、収まらなければ元の綴り。
fn number_text(n: &JsonNumber) -> String {
    match n {
        JsonNumber::PosInt(v) => v.to_string(),
        JsonNumber::NegInt(v) => v.to_string(),
        JsonNumber::Float { text, .. } => match num::from_json_number(n) {
            Some(d) => d.to_string(),
            None => text.to_string(),
        },
    }
}

/// PostgreSQL の空白（`isspace`）の前後除去。
fn trim_pg_space(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r'))
}

/// 数字列の `_` 区切りを検査して除去する（`_` は数字の間に 1 個だけ置ける）。
fn strip_underscores(run: &str) -> Option<String> {
    let b = run.as_bytes();
    if b.first() == Some(&b'_') || b.last() == Some(&b'_') || run.contains("__") {
        return None;
    }
    Some(run.replace('_', ""))
}

/// 符号の後ろの `0x`／`0o`／`0b` 接頭辞つき整数を読む（`int8in`／`numeric_in` と同じ）。
/// 接頭辞が無ければ `None`（呼び出し側が 10 進として扱う）。接頭辞があって不正なら
/// `Some(None)`。
fn parse_radix_int(rest: &str) -> Option<Option<u128>> {
    let radix = match rest.get(..2)? {
        "0x" | "0X" => 16,
        "0o" | "0O" => 8,
        "0b" | "0B" => 2,
        _ => return None,
    };
    let digits = rest.get(2..).unwrap_or("");
    let Some(clean) = strip_underscores(digits) else {
        return Some(None);
    };
    if clean.is_empty() {
        return Some(None);
    }
    Some(u128::from_str_radix(&clean, radix).ok())
}

/// `int8in`／`int4in` 相当: 前後の空白・符号・10 進数字（`_` 区切り可）または `0x`／`0o`／
/// `0b` 接頭辞つきの整数。
fn parse_int_text(s: &str) -> Option<i128> {
    let t = trim_pg_space(s);
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let mag: u128 = match parse_radix_int(rest) {
        Some(v) => v?,
        None => {
            let clean = strip_underscores(rest)?;
            if clean.is_empty() || !clean.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            clean.parse::<u128>().ok()?
        }
    };
    let v = i128::try_from(mag).ok()?;
    if neg {
        v.checked_neg()
    } else {
        Some(v)
    }
}

/// 10 進の綴りから [`JsonNumber`] を作る。`i128` に収まる値は正規の 10 進（`1e2` → `100`）、
/// 収まらない値は綴りのまま保持する。
fn number_from_text(text: String) -> JsonNumber {
    let value = text.parse::<f64>().unwrap_or(0.0);
    let raw = JsonNumber::Float {
        value,
        text: text.into_boxed_str(),
    };
    match num::from_json_number(&raw) {
        Some(d) => num::to_json_number(d),
        None => raw,
    }
}

/// `numeric_in` 相当（NaN・Infinity は拒否）。前後の空白・符号・小数・指数・`_` 区切り・
/// `0x`／`0o`／`0b` 接頭辞つき整数を受理し、RFC 8259 として読める綴りの [`JsonNumber`]
/// にする。
fn parse_numeric_text(s: &str) -> Option<JsonNumber> {
    let t = trim_pg_space(s);
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if let Some(radix) = parse_radix_int(rest) {
        let mag = radix?;
        let v = i128::try_from(mag).ok()?;
        let v = if neg { v.checked_neg()? } else { v };
        return Some(num::to_json_number(Decimal::from_parts(v, 0).ok()?));
    }
    let b = rest.as_bytes();
    let mut i = 0usize;
    while matches!(b.get(i), Some(b'0'..=b'9') | Some(b'_')) {
        i += 1;
    }
    let int_run = rest.get(..i)?;
    let mut frac_run = "";
    if b.get(i) == Some(&b'.') {
        i += 1;
        let fs = i;
        while matches!(b.get(i), Some(b'0'..=b'9') | Some(b'_')) {
            i += 1;
        }
        frac_run = rest.get(fs..i)?;
    }
    let int_part = strip_underscores(int_run)?;
    let frac_part = strip_underscores(frac_run)?;
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    let mut exp_text = String::new();
    if matches!(b.get(i), Some(b'e') | Some(b'E')) {
        i += 1;
        let mut sign = "";
        match b.get(i) {
            Some(b'+') => i += 1,
            Some(b'-') => {
                sign = "-";
                i += 1;
            }
            _ => {}
        }
        let ds = i;
        while matches!(b.get(i), Some(b'0'..=b'9') | Some(b'_')) {
            i += 1;
        }
        let digits = strip_underscores(rest.get(ds..i)?)?;
        if digits.is_empty() || digits.len() > 7 {
            return None;
        }
        exp_text = format!("{sign}{digits}");
    }
    if i != b.len() {
        return None;
    }
    // PostgreSQL の numeric が扱える桁数の目安（整数部 131072 桁）を超える指数は拒否する。
    if let Ok(e) = exp_text.parse::<i64>() {
        if e.abs() > 131_072 {
            return None;
        }
    }
    let mut text = String::new();
    if neg {
        text.push('-');
    }
    // RFC 8259 は整数部の先頭ゼロと不正な綴りを許さない。Decimal へ変換できない場合の
    // フォールバック（text をそのまま保持）でも有効な JSON になるよう、常に正規化する。
    let int_norm = int_part.trim_start_matches('0');
    text.push_str(if int_norm.is_empty() { "0" } else { int_norm });
    if !frac_part.is_empty() {
        text.push('.');
        text.push_str(&frac_part);
    }
    if !exp_text.is_empty() {
        text.push('e');
        text.push_str(&exp_text);
    }
    // 指数部を含めた綴りが数値リテラル長の上限（1,024）を超える入力は受理しない（fail-closed）。
    if text.len() > 1024 {
        return None;
    }
    Some(number_from_text(text))
}

/// `float8in` 相当で文字列を `f64` にする（有限値のみ。0 への桁落ち〔アンダーフロー〕は
/// 拒否）。10 進の浮動小数点と、符号つきの `0x` 接頭辞つき整数を受理する。
fn parse_float8_text(s: &str) -> Option<f64> {
    let t = trim_pg_space(s);
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let f = if matches!(rest.get(..2), Some("0x") | Some("0X")) {
        let digits = rest.get(2..)?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u128::from_str_radix(digits, 16).ok()? as f64
    } else {
        // `inf`／`nan` など `f64` の独自綴りは下の有限判定で弾く。符号は自前で処理済みのため、
        // 二重符号（`--1`）は拒否する。
        if rest.starts_with(['+', '-']) {
            return None;
        }
        let v = rest.parse::<f64>().ok()?;
        let has_nonzero_digit = rest
            .bytes()
            .take_while(|b| !matches!(b, b'e' | b'E'))
            .any(|b| (b'1'..=b'9').contains(&b));
        if v == 0.0 && has_nonzero_digit {
            return None;
        }
        v
    };
    if !f.is_finite() {
        return None;
    }
    Some(if neg { -f } else { f })
}

/// `float8` → `numeric` 変換（有効数字 15 桁へ丸めた平坦な 10 進）。`.double()` が文字列
/// 入力に対して返す値で、PostgreSQL の `float8_numeric` と同じ丸めになる。
fn float8_to_number(f: f64) -> JsonNumber {
    if f == 0.0 {
        return JsonNumber::PosInt(0);
    }
    let sci = format!("{:.14e}", f.abs());
    let (mant, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i64 = exp.parse().unwrap_or(0);
    let mut digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let point = exp + 1;
    let len = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let mut text = String::new();
    if f < 0.0 {
        text.push('-');
    }
    if point <= 0 {
        text.push_str("0.");
        for _ in 0..(-point) {
            text.push('0');
        }
        text.push_str(&digits);
    } else if point >= len {
        text.push_str(&digits);
        for _ in 0..(point - len) {
            text.push('0');
        }
    } else {
        let p = usize::try_from(point).unwrap_or(0);
        text.push_str(digits.get(..p).unwrap_or(""));
        text.push('.');
        text.push_str(digits.get(p..).unwrap_or(""));
    }
    number_from_text(text)
}

/// `parse_bool_with_len` 相当: 一意に決まる接頭辞を受理する（空白は許さない）。
fn parse_bool_text(s: &str) -> Option<bool> {
    let l = s.to_ascii_lowercase();
    let prefix_of = |word: &str| !l.is_empty() && word.starts_with(l.as_str());
    match l.as_bytes().first()? {
        b't' if prefix_of("true") => Some(true),
        b'f' if prefix_of("false") => Some(false),
        b'y' if prefix_of("yes") => Some(true),
        b'n' if prefix_of("no") => Some(false),
        b'o' if l.len() >= 2 && "on".starts_with(l.as_str()) => Some(true),
        b'o' if l.len() >= 2 && "off".starts_with(l.as_str()) => Some(false),
        b'1' if l.len() == 1 => Some(true),
        b'0' if l.len() == 1 => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 公開 API
// ---------------------------------------------------------------------------

/// 項目を所有の [`JsonValue`] にする。部分木の複製は大きさ（ノード数）に比例して課金する。
fn materialize(ev: &mut Ev<'_>, item: Item<'_>) -> Result<JsonValue, JsonPathEvalError> {
    let into = |ev: &mut Ev<'_>, v: &JsonValue| -> Result<JsonValue, JsonPathEvalError> {
        // 明示スタックでノード数とバイト数を数えて課金してから複製する。
        let mut n: u64 = 0;
        let mut bytes: u64 = 0;
        let mut stack: Vec<&JsonValue> = vec![v];
        while let Some(x) = stack.pop() {
            n = n.saturating_add(1);
            bytes = bytes.saturating_add(NODE_COST_BYTES);
            match x {
                JsonValue::Array(a) => stack.extend(a.iter()),
                JsonValue::Object(m) => {
                    for (k, c) in m {
                        bytes = bytes.saturating_add(u64::try_from(k.len()).unwrap_or(u64::MAX));
                        stack.push(c);
                    }
                }
                JsonValue::String(s) => {
                    bytes = bytes.saturating_add(u64::try_from(s.len()).unwrap_or(u64::MAX));
                }
                _ => {}
            }
        }
        ev.tick(n).map_err(unwrap_fail)?;
        ev.charge_bytes(bytes).map_err(unwrap_fail)?;
        Ok(v.clone())
    };
    match item {
        Item::Ref(v) => into(ev, v),
        Item::Null => Ok(JsonValue::Null),
        Item::Bool(b) => Ok(JsonValue::Bool(b)),
        Item::Num(n) => Ok(JsonValue::Number(n)),
        Item::Str(s) => {
            ev.charge_bytes(u64::try_from(s.len()).unwrap_or(u64::MAX))
                .map_err(unwrap_fail)?;
            Ok(JsonValue::String(s.into_owned()))
        }
        Item::KeyValue { id, key, value } => {
            ev.charge_bytes(u64::try_from(key.len()).unwrap_or(u64::MAX))
                .map_err(unwrap_fail)?;
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), JsonValue::Number(int_number(id)));
            m.insert("key".to_string(), JsonValue::String(key.to_string()));
            m.insert("value".to_string(), into(ev, value)?);
            Ok(JsonValue::Object(m))
        }
    }
}

/// `Fail` を公開エラーへ。評価の外へ出る時点では `Error`（抑止）は残らない想定だが、
/// 残った場合も fail-closed に内部エラーとして返す。
fn unwrap_fail(f: Fail) -> JsonPathEvalError {
    match f {
        Fail::Thrown(e) => e,
        Fail::Error => JsonPathEvalError::Internal,
    }
}

/// 実行の共通部。`collect_all` は結果列が必要か（存在判定の lax だけ `false`）。
/// 戻り値は `(抑止されたエラーで終わったか, 項目列, 評価器)`。
fn run<'a>(
    path: &'a SqlJsonPath,
    root: &'a JsonValue,
    opts: &JsonPathEvalOptions<'a>,
    collect_all: bool,
) -> Result<(bool, Vec<Item<'a>>, Ev<'a>), JsonPathEvalError> {
    let vars = match opts.vars {
        None => None,
        Some(JsonValue::Object(m)) => Some(m),
        Some(_) => return Err(JsonPathEvalError::VarsNotObject),
    };
    let lax = path.mode == PathMode::Lax;
    let mut ev = Ev {
        root,
        vars,
        lax,
        throw: !opts.silent,
        ignore_structural: lax,
        current: Item::Ref(root),
        innermost: -1,
        steps_left: opts.limits.max_eval_steps(),
        bytes_left: MAX_EVAL_ALLOC_BYTES,
        depth: 0,
        max_depth: opts.limits.max_eval_depth(),
        id_map: None,
    };
    let mut items: Vec<Item<'a>> = Vec::new();
    let root_item = Item::Ref(root);
    let want_list = collect_all || !lax;
    let r = if want_list {
        ev.exec_item(&path.expr, &root_item, Some(&mut items))
    } else {
        ev.exec_item(&path.expr, &root_item, None)
    };
    match r {
        Ok(res) => {
            if !want_list {
                // 存在判定（lax）: 最初の 1 件の有無を `items` に 1 件で表す。
                if res == Res::Ok {
                    items.push(Item::Null);
                }
            }
            Ok((false, items, ev))
        }
        Err(Fail::Error) => Ok((true, items, ev)),
        Err(Fail::Thrown(e)) => Err(e),
    }
}

/// jsonpath を評価して結果の項目列を返す（`jsonb_path_query` 相当）。`silent` で抑止された
/// エラーは、それまでに得た項目だけを返す（PostgreSQL と同じ）。
pub fn jsonpath_query(
    path: &SqlJsonPath,
    root: &JsonValue,
    opts: JsonPathEvalOptions<'_>,
) -> Result<Vec<JsonValue>, JsonPathEvalError> {
    let (_suppressed, items, mut ev) = run(path, root, &opts, true)?;
    let mut out = Vec::with_capacity(items.len().min(1024));
    for it in items {
        out.push(materialize(&mut ev, it)?);
    }
    Ok(out)
}

/// 存在判定（`jsonb_path_exists`・`@?` 相当）。抑止されたエラーは `None`（SQL の NULL）。
pub fn jsonpath_exists(
    path: &SqlJsonPath,
    root: &JsonValue,
    opts: JsonPathEvalOptions<'_>,
) -> Result<Option<bool>, JsonPathEvalError> {
    let (suppressed, items, _ev) = run(path, root, &opts, false)?;
    if suppressed {
        return Ok(None);
    }
    Ok(Some(!items.is_empty()))
}

/// 真偽判定（`jsonb_path_match`・`@@` 相当）。結果が単一の真偽値なら `Some(b)`、単一の
/// null なら `None`、それ以外は `22038`（`silent` なら `None`）。
pub fn jsonpath_match(
    path: &SqlJsonPath,
    root: &JsonValue,
    opts: JsonPathEvalOptions<'_>,
) -> Result<Option<bool>, JsonPathEvalError> {
    let silent = opts.silent;
    let (suppressed, items, _ev) = run(path, root, &opts, true)?;
    // 抑止されたエラーで終わった場合は、途中に集まった項目に依らず NULL（silent 時のみ到達）。
    if suppressed {
        return Ok(None);
    }
    if let [one] = items.as_slice() {
        match one {
            Item::Bool(b) | Item::Ref(JsonValue::Bool(b)) => return Ok(Some(*b)),
            Item::Null | Item::Ref(JsonValue::Null) => return Ok(None),
            _ => {}
        }
    }
    if silent {
        Ok(None)
    } else {
        Err(JsonPathEvalError::BooleanResultRequired)
    }
}
