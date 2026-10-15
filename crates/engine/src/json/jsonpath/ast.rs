//! SQL/JSON path（`jsonpath`）の抽象構文木（AST）。
//!
//! [`super::parse_jsonpath`] が成功したときだけ [`SqlJsonPath`] が得られる。後続の評価器
//! （Issue #2184）・`jsonb_path_*` 関数（#2185）・`@?`／`@@`（#2186）・`JSON_EXISTS` 等
//! （#2187）は、この AST を前提にする（TABLE-25 ポインタ）。
//!
//! 数値リテラルは任意精度の 10 進正規化テキスト（[`PathNumeric`]）で保持する。`numeric`
//! モジュールの `Decimal`（38 桁）では `1e400` のような値を表せないためで、jsonb の
//! 数値と同じ桁文字列比較の経路へ評価器が接続できるよう RFC 8259 として読める形に
//! そろえている。連続したアクセサ（`$.a.b[0]`）は [`PathNode::Path`] の `ops` へ平坦に
//! 持ち、AST の入れ子深さが入力長に比例して深くなることを避ける。

/// `lax`／`strict` モード（省略時は `Lax`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    Lax,
    Strict,
}

/// 解析済みの jsonpath 全体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlJsonPath {
    pub mode: PathMode,
    pub expr: PathNode,
}

/// 数値リテラル。`_` 区切りと基数つき整数を除去した 10 進テキスト（例: `0x1F` は
/// `31`、`.1` は `0.1`、`1.` は `1`、`1.e3` は `1e3`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathNumeric(pub(crate) Box<str>);

impl PathNumeric {
    /// 正規化済みの 10 進テキスト。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 単項演算子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
}

/// 二項算術演算子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// 比較演算子（`<>` は `Ne` へ正規化する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// `starts with` の右辺（文字列または変数のみ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartsWithOperand {
    String(Box<str>),
    Variable(Box<str>),
}

/// `like_regex` のフラグ。解析は行うが、現状は 0A000 で拒否するため評価器には届かない。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegexFlags {
    pub case_insensitive: bool,
    pub dot_all: bool,
    pub multiline: bool,
    pub literal: bool,
    pub extended: bool,
}

/// 式・述語のノード。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathNode {
    Root,
    Current,
    Last,
    Variable(Box<str>),
    Null,
    Bool(bool),
    String(Box<str>),
    Numeric(PathNumeric),
    /// アクセサの連鎖。`ops` は 1 個以上で、左から順に適用する。
    Path {
        base: Box<PathNode>,
        ops: Vec<AccessorOp>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<PathNode>,
    },
    Arith {
        op: ArithOp,
        lhs: Box<PathNode>,
        rhs: Box<PathNode>,
    },
    Compare {
        op: CompareOp,
        lhs: Box<PathNode>,
        rhs: Box<PathNode>,
    },
    And(Box<PathNode>, Box<PathNode>),
    Or(Box<PathNode>, Box<PathNode>),
    Not(Box<PathNode>),
    IsUnknown(Box<PathNode>),
    Exists(Box<PathNode>),
    StartsWith {
        lhs: Box<PathNode>,
        prefix: StartsWithOperand,
    },
    LikeRegex {
        lhs: Box<PathNode>,
        pattern: Box<str>,
        flags: RegexFlags,
    },
}

/// 配列添字 1 要素（`from` のみ、または `from to to`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscript {
    pub from: PathNode,
    pub to: Option<PathNode>,
}

/// `.**{a to b}` の端点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnyLevel {
    Level(u32),
    Last,
}

/// アクセサ 1 段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessorOp {
    Member(Box<str>),
    MemberWildcard,
    ArrayWildcard,
    Subscripts(Vec<Subscript>),
    /// `.**`（`0..last`）・`.**{n}`・`.**{n to m}`。
    AnyPath {
        from: AnyLevel,
        to: AnyLevel,
    },
    Method(Method),
    Filter(Box<PathNode>),
}

/// 項目メソッド。引数の整数は符号つきの 10 進正規化テキストで、範囲検査は評価時
/// （PostgreSQL と同じく解析段階では検査しない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Abs,
    Size,
    Type,
    Floor,
    Ceiling,
    Double,
    KeyValue,
    BigInt,
    Boolean,
    Date,
    Integer,
    Number,
    String,
    Datetime(Option<Box<str>>),
    Decimal {
        precision: Option<Box<str>>,
        scale: Option<Box<str>>,
    },
    Time(Option<Box<str>>),
    TimeTz(Option<Box<str>>),
    Timestamp(Option<Box<str>>),
    TimestampTz(Option<Box<str>>),
}
