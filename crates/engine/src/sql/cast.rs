//! `CAST(<expr> AS <type>)` の型・規則表・束縛（Issue #1590・SQL-47 のポインタ）。
//!
//! 責務境界: 構文段（`sql::allowlist::Parser::parse_cast_expr`）が作る [`Expr::Cast`] を、
//! 束縛段（`sql::udf_call::bind_expr_any` の `Expr::Cast` 腕）が [`bind_cast`] で
//! [`BoundExpr`] へ落とす。変換先の型名は DDL と同じ型名解決
//! （`sql::ddl_column_type::parse_column_type_name`）で読み、[`CastType::from_sql_type_name`]
//! で CAST 用の閉じた型集合へ写す（カタログは参照しない静的な判定）。
//!
//! 規則表（20 型 × 20 型）は `docs/design/cast-conversion-rules.md` の表をデータとして
//! 持つ（[`cast_rule`]）。型の組だけで決まる `×`（`42846`）は束縛時に判定する。値の変換は
//! `BuiltinFn::Cast`（評価は `sql::cast_value`）として束縛する。数値 6 型・BOOLEAN・文字列 3 型
//! どうし（Issue #1591）に加え、日時・JSON・UUID・BYTEA・配列（Issue #1592）も同じ経路で扱う。
//! 配列は要素型の組で規則を引き直し（[`cast_rule_types`]）、行内表現を持つ要素型
//! （[`CastType::array_elem_type`]）だけを変換先にできる。実装の無い組（対応する要素型の無い配列・
//! 種別を静的に決められない数値）は `0A000` で拒否する（黙って別の型として評価しない。fail-closed）。
//! 判定の順序は「型名の解決（`42704`／`42601`／`22023`／`0A000`）→ 型の組（`42846`）→ 実装の有無（`0A000`）」。

use crate::catalog::{ArrayElemType, ArrayType, ColumnType, TableSchema, MAX_ARRAY_ELEMENTS};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast_value::{CastDst, CastFn, CastSrc};
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::sql::udf_call::{
    bind_expr_any, bind_jsonb_literal, BindEnv, BoundExpr, BuiltinFn, Expr, ExprType, NumericKind,
};

/// 修飾子なしの `CAST(.. AS numeric)` の結果列型。セルは値ごとの scale を持つ `Decimal` で、
/// wire の typmod は numeric で常に -1 のため scale は外部に出ない。scale を 0 以外にするのは、
/// サブクエリが scale 0 の numeric を整数とみなす判定を避けるため（`AVG_INTEGER_RESULT_TYPE`
/// と同じ前例）。
pub(crate) const CAST_NUMERIC_RESULT_TYPE: ColumnType = ColumnType::Numeric {
    precision: crate::numeric::MAX_PRECISION,
    scale: 16,
};

/// numeric の型修飾子 `(p, s)` の上限（PostgreSQL 17 は精度 1..=1000）。
const PG_NUMERIC_MAX_PRECISION: u32 = 1000;

/// 変換先（または変換元）の型。CAST が扱う 20 型（配列は要素型つきで 1 種）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastType {
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    /// `None` は修飾子なし（任意精度。丸めない）。`Some((precision, scale))` は `numeric(p,s)`。
    Numeric(Option<(u8, u8)>),
    Bool,
    Text,
    /// `None` は長さ修飾なし。
    Varchar(Option<u32>),
    Bpchar(u32),
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    Json,
    Jsonb,
    Uuid,
    Bytea,
    Array(Box<CastType>),
}

/// 規則表の行・列の見出し（20 種。配列は要素型によらず 1 種）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastKind {
    Int4,
    Int8,
    Int2,
    Float4,
    Float8,
    Numeric,
    Bool,
    Text,
    Varchar,
    Bpchar,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    Json,
    Jsonb,
    Uuid,
    Bytea,
    Array,
}

/// 規則表の 1 セル（凡例は設計書「規則表」参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastRule {
    /// `=` 同一型（型修飾子の適用のみ）。
    Identity,
    /// `i` 暗黙。
    Implicit,
    /// `a` 代入。
    Assignment,
    /// `e` 明示。
    Explicit,
    /// `io/a` 入出力変換（各型 → 文字列型）。
    IoAssignment,
    /// `io/e` 入出力変換（文字列型 → 各型）。
    IoExplicit,
    /// `elem` 配列 → 配列（要素型の規則に従う）。
    Element,
    /// `×` 変換の対応なし。
    None,
}

impl CastKind {
    /// 表の添字（行・列の並びは設計書の表と同じ）。
    const fn index(self) -> usize {
        self as usize
    }

    /// `42846` の文言に使う PostgreSQL の型名（静的な語のみ）。
    pub fn label(self) -> &'static str {
        match self {
            CastKind::Int4 => "integer",
            CastKind::Int8 => "bigint",
            CastKind::Int2 => "smallint",
            CastKind::Float4 => "real",
            CastKind::Float8 => "double precision",
            CastKind::Numeric => "numeric",
            CastKind::Bool => "boolean",
            CastKind::Text => "text",
            CastKind::Varchar => "character varying",
            CastKind::Bpchar => "character",
            CastKind::Date => "date",
            CastKind::Time => "time without time zone",
            CastKind::Timestamp => "timestamp without time zone",
            CastKind::TimestampTz => "timestamp with time zone",
            CastKind::Interval => "interval",
            CastKind::Json => "json",
            CastKind::Jsonb => "jsonb",
            CastKind::Uuid => "uuid",
            CastKind::Bytea => "bytea",
            CastKind::Array => "anyarray",
        }
    }
}

const EQ: CastRule = CastRule::Identity;
const IM: CastRule = CastRule::Implicit;
const AS: CastRule = CastRule::Assignment;
const EX: CastRule = CastRule::Explicit;
const IA: CastRule = CastRule::IoAssignment;
const IE: CastRule = CastRule::IoExplicit;
const EL: CastRule = CastRule::Element;
const NO: CastRule = CastRule::None;

/// 規則表（行が変換元、列が変換先。並びは [`CastKind`] の宣言順）。
/// `docs/design/cast-conversion-rules.md` の表と一致することを単体テストで 400 セル照合する。
#[rustfmt::skip]
const RULES: [[CastRule; 20]; 20] = [
    // int4
    [EQ, IM, AS, IM, IM, IM, EX, IA, IA, IA, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO],
    // int8
    [AS, EQ, AS, IM, IM, IM, NO, IA, IA, IA, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO],
    // int2
    [IM, IM, EQ, IM, IM, IM, NO, IA, IA, IA, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO],
    // float4
    [AS, AS, AS, EQ, IM, AS, NO, IA, IA, IA, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO],
    // float8
    [AS, AS, AS, AS, EQ, AS, NO, IA, IA, IA, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO],
    // numeric
    [AS, AS, AS, IM, IM, EQ, NO, IA, IA, IA, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO],
    // bool
    [EX, NO, NO, NO, NO, NO, EQ, AS, AS, AS, NO, NO, NO, NO, NO, NO, NO, NO, NO, NO],
    // text
    [IE, IE, IE, IE, IE, IE, IE, EQ, IM, IM, IE, IE, IE, IE, IE, IE, IE, IE, IE, IE],
    // varchar
    [IE, IE, IE, IE, IE, IE, IE, IM, EQ, IM, IE, IE, IE, IE, IE, IE, IE, IE, IE, IE],
    // bpchar
    [IE, IE, IE, IE, IE, IE, IE, IM, IM, EQ, IE, IE, IE, IE, IE, IE, IE, IE, IE, IE],
    // date
    [NO, NO, NO, NO, NO, NO, NO, IA, IA, IA, EQ, NO, IM, IM, NO, NO, NO, NO, NO, NO],
    // time
    [NO, NO, NO, NO, NO, NO, NO, IA, IA, IA, NO, EQ, NO, NO, IM, NO, NO, NO, NO, NO],
    // timestamp
    [NO, NO, NO, NO, NO, NO, NO, IA, IA, IA, AS, AS, EQ, IM, NO, NO, NO, NO, NO, NO],
    // timestamptz
    [NO, NO, NO, NO, NO, NO, NO, IA, IA, IA, AS, AS, AS, EQ, NO, NO, NO, NO, NO, NO],
    // interval
    [NO, NO, NO, NO, NO, NO, NO, IA, IA, IA, NO, AS, NO, NO, EQ, NO, NO, NO, NO, NO],
    // json
    [NO, NO, NO, NO, NO, NO, NO, IA, IA, IA, NO, NO, NO, NO, NO, EQ, AS, NO, NO, NO],
    // jsonb
    [EX, EX, EX, EX, EX, EX, EX, IA, IA, IA, NO, NO, NO, NO, NO, AS, EQ, NO, NO, NO],
    // uuid
    [NO, NO, NO, NO, NO, NO, NO, IA, IA, IA, NO, NO, NO, NO, NO, NO, NO, EQ, NO, NO],
    // bytea
    [NO, NO, NO, NO, NO, NO, NO, IA, IA, IA, NO, NO, NO, NO, NO, NO, NO, NO, EQ, NO],
    // array
    [NO, NO, NO, NO, NO, NO, NO, IA, IA, IA, NO, NO, NO, NO, NO, NO, NO, NO, NO, EL],
];

/// 型の種別（要素型を問わない）の組から規則表のセルを引く。
pub fn cast_rule(from: CastKind, to: CastKind) -> CastRule {
    RULES
        .get(from.index())
        .and_then(|row| row.get(to.index()))
        .copied()
        // 添字は enum の判別値で範囲内に収まる。到達しない分岐だが、万一の場合は拒否側。
        .unwrap_or(CastRule::None)
}

/// 型（配列は要素型つき）の組から規則を引く。配列 → 配列は要素型の組で判定し直し、
/// 要素の組が `×` なら `×`、そうでなければ `elem`。
pub fn cast_rule_types(from: &CastType, to: &CastType) -> CastRule {
    match (from, to) {
        (CastType::Array(a), CastType::Array(b)) => {
            if cast_rule_types(a, b) == CastRule::None {
                CastRule::None
            } else {
                CastRule::Element
            }
        }
        _ => cast_rule(from.kind(), to.kind()),
    }
}

impl CastType {
    /// 規則表の見出し。
    pub fn kind(&self) -> CastKind {
        match self {
            CastType::Int2 => CastKind::Int2,
            CastType::Int4 => CastKind::Int4,
            CastType::Int8 => CastKind::Int8,
            CastType::Float4 => CastKind::Float4,
            CastType::Float8 => CastKind::Float8,
            CastType::Numeric(_) => CastKind::Numeric,
            CastType::Bool => CastKind::Bool,
            CastType::Text => CastKind::Text,
            CastType::Varchar(_) => CastKind::Varchar,
            CastType::Bpchar(_) => CastKind::Bpchar,
            CastType::Date => CastKind::Date,
            CastType::Time => CastKind::Time,
            CastType::Timestamp => CastKind::Timestamp,
            CastType::TimestampTz => CastKind::TimestampTz,
            CastType::Interval => CastKind::Interval,
            CastType::Json => CastKind::Json,
            CastType::Jsonb => CastKind::Jsonb,
            CastType::Uuid => CastKind::Uuid,
            CastType::Bytea => CastKind::Bytea,
            CastType::Array(_) => CastKind::Array,
        }
    }

    /// DDL の型名構文木から CAST 用の型へ写す（カタログ非参照の純関数）。
    ///
    /// - 別名（`int4`・`float8` 等）は組み込み型へ解決する。PostgreSQL と同じく組み込み型を優先し、
    ///   同名の ENUM は見ない。
    /// - ENUM 型名の候補は `42704`（カタログを引かないため、他テナントの型の存在オラクルにならない）。
    /// - `VECTOR` は規則表の外のため `0A000`。
    /// - 配列は要素を写し、`[N]` の `N` は PostgreSQL と同じく捨てる。
    pub fn from_sql_type_name(ty: &SqlColumnTypeName) -> Result<CastType, SqlSurfaceError> {
        Ok(match ty {
            SqlColumnTypeName::Text => CastType::Text,
            SqlColumnTypeName::Varchar(len) => CastType::Varchar(*len),
            SqlColumnTypeName::Bpchar(len) => CastType::Bpchar(*len),
            SqlColumnTypeName::SmallInt => CastType::Int2,
            SqlColumnTypeName::Integer => CastType::Int4,
            SqlColumnTypeName::BigInt => CastType::Int8,
            SqlColumnTypeName::Real => CastType::Float4,
            SqlColumnTypeName::Double => CastType::Float8,
            SqlColumnTypeName::Boolean => CastType::Bool,
            SqlColumnTypeName::Date => CastType::Date,
            SqlColumnTypeName::Timestamp => CastType::Timestamp,
            SqlColumnTypeName::Time => CastType::Time,
            SqlColumnTypeName::TimestampTz => CastType::TimestampTz,
            SqlColumnTypeName::Interval => CastType::Interval,
            SqlColumnTypeName::Bytea => CastType::Bytea,
            SqlColumnTypeName::Json => CastType::Json,
            SqlColumnTypeName::Jsonb => CastType::Jsonb,
            SqlColumnTypeName::Uuid => CastType::Uuid,
            SqlColumnTypeName::Numeric { precision, scale } => CastType::Numeric(Some(
                numeric_typmod(u32::from(*precision), i64::from(*scale))?,
            )),
            SqlColumnTypeName::Vector(_) => {
                return Err(SqlSurfaceError::FeatureNotSupported {
                    detail: "cast to or from type vector is not supported".to_string(),
                })
            }
            SqlColumnTypeName::Alias { target, .. } => return CastType::from_sql_type_name(target),
            SqlColumnTypeName::Enum(name) => {
                return Err(SqlSurfaceError::UndefinedType { name: name.clone() })
            }
            SqlColumnTypeName::Array { elem, .. } => {
                CastType::Array(Box::new(CastType::from_sql_type_name(elem)?))
            }
        })
    }

    /// 再解析できる正規の綴り（CHECK の描画・content_hash の直列化で共有する）。
    /// 固定語彙だけから組み立て、利用者の綴りは連結しない。
    pub fn to_sql(&self) -> String {
        match self {
            CastType::Int2 => "smallint".to_string(),
            CastType::Int4 => "integer".to_string(),
            CastType::Int8 => "bigint".to_string(),
            CastType::Float4 => "real".to_string(),
            CastType::Float8 => "double precision".to_string(),
            CastType::Numeric(Some((precision, scale))) => format!("numeric({precision},{scale})"),
            CastType::Numeric(None) => "numeric".to_string(),
            CastType::Bool => "boolean".to_string(),
            CastType::Text => "text".to_string(),
            CastType::Varchar(None) => "character varying".to_string(),
            CastType::Varchar(Some(n)) => format!("character varying({n})"),
            CastType::Bpchar(n) => format!("character({n})"),
            CastType::Date => "date".to_string(),
            CastType::Time => "time".to_string(),
            CastType::Timestamp => "timestamp".to_string(),
            CastType::TimestampTz => "timestamp with time zone".to_string(),
            CastType::Interval => "interval".to_string(),
            CastType::Json => "json".to_string(),
            CastType::Jsonb => "jsonb".to_string(),
            CastType::Uuid => "uuid".to_string(),
            CastType::Bytea => "bytea".to_string(),
            CastType::Array(elem) => format!("{}[]", elem.to_sql()),
        }
    }

    /// エラー文言用の PostgreSQL の型名（型修飾子なし・静的な語のみ）。
    pub fn pg_label(&self) -> String {
        match self {
            CastType::Array(elem) => format!("{}[]", elem.pg_label()),
            other => other.kind().label().to_string(),
        }
    }

    /// PostgreSQL のカタログ上の型名（`int4`・`timestamptz` 等）。既定の列名（`CAST(1 AS integer)` の
    /// 列名は `int4`）に使う。配列は要素型の名前（PostgreSQL の既定列名と同じ）。
    pub fn catalog_name(&self) -> &'static str {
        match self {
            CastType::Int2 => "int2",
            CastType::Int4 => "int4",
            CastType::Int8 => "int8",
            CastType::Float4 => "float4",
            CastType::Float8 => "float8",
            CastType::Numeric(_) => "numeric",
            CastType::Bool => "bool",
            CastType::Text => "text",
            CastType::Varchar(_) => "varchar",
            CastType::Bpchar(_) => "bpchar",
            CastType::Date => "date",
            CastType::Time => "time",
            CastType::Timestamp => "timestamp",
            CastType::TimestampTz => "timestamptz",
            CastType::Interval => "interval",
            CastType::Json => "json",
            CastType::Jsonb => "jsonb",
            CastType::Uuid => "uuid",
            CastType::Bytea => "bytea",
            CastType::Array(elem) => elem.catalog_name(),
        }
    }

    /// 型修飾子（長さ・精度）を持つか。修飾子の適用は値の変換（丸め・切り詰め・空白埋め）を
    /// 伴うため、同一型でも素通しにしない。
    pub fn has_modifier(&self) -> bool {
        matches!(
            self,
            CastType::Varchar(Some(_)) | CastType::Bpchar(_) | CastType::Numeric(Some(_))
        )
    }

    /// 配列の変換先・変換元としての要素型（`CastType::Array(elem)` の `elem`）を、行内表現を
    /// 持つ `ArrayElemType` へ写す。対応が無い要素（`smallint`・`time`・`timestamptz`・
    /// `interval`・`varchar(n)`・`char(n)`・修飾子なしの `numeric`・ENUM）と、配列でない型は
    /// `None`（束縛段が `0A000` にする。fail-closed）。
    pub fn array_elem_type(&self) -> Option<ArrayElemType> {
        let CastType::Array(elem) = self else {
            return None;
        };
        Some(match elem.as_ref() {
            CastType::Text => ArrayElemType::Text,
            CastType::Bool => ArrayElemType::Bool,
            CastType::Int4 => ArrayElemType::Integer,
            CastType::Int8 => ArrayElemType::BigInt,
            CastType::Float4 => ArrayElemType::Real,
            CastType::Float8 => ArrayElemType::Double,
            CastType::Date => ArrayElemType::Date,
            CastType::Timestamp => ArrayElemType::Timestamp,
            CastType::Uuid => ArrayElemType::Uuid,
            CastType::Numeric(Some((precision, scale))) => ArrayElemType::Numeric {
                precision: *precision,
                scale: *scale,
            },
            CastType::Bytea => ArrayElemType::Bytea,
            CastType::Json => ArrayElemType::Json,
            CastType::Jsonb => ArrayElemType::Jsonb,
            _ => return None,
        })
    }

    /// 束縛段の式の型。対応が無い型（対応する要素型の無い配列）は `None`。
    pub fn expr_type(&self) -> Option<ExprType> {
        match self {
            CastType::Int2
            | CastType::Int4
            | CastType::Int8
            | CastType::Float4
            | CastType::Float8 => Some(ExprType::Scalar),
            CastType::Bool => Some(ExprType::Bool),
            CastType::Text | CastType::Varchar(_) | CastType::Bpchar(_) => Some(ExprType::Text),
            CastType::Date => Some(ExprType::Date),
            CastType::Time => Some(ExprType::Time),
            CastType::Timestamp => Some(ExprType::Timestamp),
            CastType::TimestampTz => Some(ExprType::TimestampTz),
            CastType::Interval => Some(ExprType::Interval),
            CastType::Jsonb => Some(ExprType::Json),
            CastType::Json => Some(ExprType::JsonText),
            CastType::Numeric(_) => Some(ExprType::Numeric),
            CastType::Uuid => Some(ExprType::Uuid),
            CastType::Bytea => Some(ExprType::Bytea),
            CastType::Array(_) => self.array_elem_type().map(|_| ExprType::Array),
        }
    }

    /// 結果列の型（RowDescription の OID の元。`udf_call::expr_result_column_type` が使う）。
    /// 式の型を持たない型は `None`（束縛段が先に `0A000` にするため通常は到達しない）。
    pub fn result_column_type(&self) -> Option<ColumnType> {
        match self {
            CastType::Int2 => Some(ColumnType::SmallInt),
            CastType::Int4 => Some(ColumnType::Integer),
            CastType::Int8 => Some(ColumnType::BigInt),
            CastType::Float4 => Some(ColumnType::Real),
            CastType::Float8 => Some(ColumnType::Double),
            CastType::Bool => Some(ColumnType::Boolean),
            CastType::Text => Some(ColumnType::Text),
            CastType::Varchar(len) => Some(ColumnType::Varchar(*len)),
            CastType::Bpchar(n) => Some(ColumnType::Bpchar(*n)),
            CastType::Date => Some(ColumnType::Date),
            CastType::Time => Some(ColumnType::Time),
            CastType::Timestamp => Some(ColumnType::Timestamp),
            CastType::TimestampTz => Some(ColumnType::TimestampTz),
            CastType::Interval => Some(ColumnType::Interval),
            CastType::Json => Some(ColumnType::Json),
            CastType::Jsonb => Some(ColumnType::Jsonb),
            CastType::Numeric(Some((precision, scale))) => Some(ColumnType::Numeric {
                precision: *precision,
                scale: *scale,
            }),
            CastType::Numeric(None) => Some(CAST_NUMERIC_RESULT_TYPE),
            CastType::Uuid => Some(ColumnType::Uuid),
            CastType::Bytea => Some(ColumnType::Bytea),
            CastType::Array(_) => {
                let elem = self.array_elem_type()?;
                ArrayType::new(elem, MAX_ARRAY_ELEMENTS)
                    .ok()
                    .map(ColumnType::Array)
            }
        }
    }

    /// 型付き算術のオペランド種別（`udf_call::numeric_kind` が使う）。倍精度は既存の慣例どおり `None`。
    pub fn numeric_kind(&self) -> Option<NumericKind> {
        match self {
            CastType::Int2 => Some(NumericKind::Int2),
            CastType::Int4 => Some(NumericKind::Int4),
            CastType::Int8 => Some(NumericKind::Int8),
            CastType::Float4 => Some(NumericKind::Float4),
            _ => None,
        }
    }
}

/// 変換元の静的な型の導出結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SourceKind {
    /// 規則表の見出しが確定した。
    Known(CastKind),
    /// 数値だが種別（整数・float・numeric）を静的に決められない
    /// （`id` 疑似列・関数・算術・CASE など）。
    UnknownNumeric,
    /// 配列。`Some` は要素型が確定したもの、`None` は要素型を `CastType` で表せない
    /// （ENUM 要素）もの。
    Array(Option<CastType>),
    /// 規則表の外（`vector`）。
    Outside,
}

/// 配列の要素型を CAST の型へ写す（ENUM は `None`）。
pub(crate) fn elem_cast_type(e: ArrayElemType) -> Option<CastType> {
    Some(match e {
        ArrayElemType::Text => CastType::Text,
        ArrayElemType::Bool => CastType::Bool,
        ArrayElemType::Integer => CastType::Int4,
        ArrayElemType::BigInt => CastType::Int8,
        ArrayElemType::Real => CastType::Float4,
        ArrayElemType::Double => CastType::Float8,
        ArrayElemType::Date => CastType::Date,
        ArrayElemType::Timestamp => CastType::Timestamp,
        ArrayElemType::Uuid => CastType::Uuid,
        ArrayElemType::Numeric { precision, scale } => CastType::Numeric(Some((precision, scale))),
        ArrayElemType::Bytea => CastType::Bytea,
        ArrayElemType::Json => CastType::Json,
        ArrayElemType::Jsonb => CastType::Jsonb,
        ArrayElemType::Enum => return None,
    })
}

fn kind_of_column(ty: &ColumnType) -> Option<CastKind> {
    match ty {
        ColumnType::SmallInt => Some(CastKind::Int2),
        ColumnType::Integer => Some(CastKind::Int4),
        ColumnType::BigInt => Some(CastKind::Int8),
        ColumnType::Real => Some(CastKind::Float4),
        ColumnType::Double => Some(CastKind::Float8),
        ColumnType::Text => Some(CastKind::Text),
        ColumnType::Varchar(_) => Some(CastKind::Varchar),
        ColumnType::Bpchar(_) => Some(CastKind::Bpchar),
        ColumnType::Numeric { .. } => Some(CastKind::Numeric),
        ColumnType::Boolean => Some(CastKind::Bool),
        ColumnType::Date => Some(CastKind::Date),
        ColumnType::Time => Some(CastKind::Time),
        ColumnType::Timestamp => Some(CastKind::Timestamp),
        ColumnType::TimestampTz => Some(CastKind::TimestampTz),
        ColumnType::Interval => Some(CastKind::Interval),
        ColumnType::Json => Some(CastKind::Json),
        ColumnType::Jsonb => Some(CastKind::Jsonb),
        ColumnType::Uuid => Some(CastKind::Uuid),
        ColumnType::Bytea => Some(CastKind::Bytea),
        ColumnType::Array(_) => Some(CastKind::Array),
        ColumnType::Vector(_) | ColumnType::Enum(_) => None,
    }
}

fn column_kind(schema: Option<&TableSchema>, index: usize) -> Option<CastKind> {
    kind_of_column(&schema?.columns.get(index)?.ty)
}

/// 型を変えない `COLLATE` ラッパーを剥がした式を返す（変換元の型追跡用）。
pub(super) fn strip_collate(mut e: &Expr) -> &Expr {
    while let Expr::Collate { expr, .. } = e {
        e = expr;
    }
    e
}

/// 変換元の静的な型を、束縛済みの式から導く。
///
/// 数値は列参照・整数／小数リテラル・型付き算術・入れ子の CAST から種別を決め、決められないものは
/// [`SourceKind::UnknownNumeric`]。TEXT は列の宣言型（text／varchar）と `BpcharToText` から復元する
/// （三者は `42846` の判定がすべて同じなので、取り違えても `×` の判定は変わらない。恒等の判定にだけ
/// 正確さが要る）。
pub(super) fn source_cast_kind(
    operand: &Expr,
    bound: &BoundExpr,
    ty: ExprType,
    schema: Option<&TableSchema>,
) -> SourceKind {
    // 入れ子の CAST の結果は、内側の変換先の型（恒等・リテラル・型付き NULL のいずれも）。
    // COLLATE は型を変えないため剥がして見る。
    if let Expr::Cast { ty: inner, .. } = strip_collate(operand) {
        return match inner {
            CastType::Array(elem) => SourceKind::Array(Some(elem.as_ref().clone())),
            other => SourceKind::Known(other.kind()),
        };
    }
    match ty {
        ExprType::Scalar => {
            if let Expr::Number(raw) = operand {
                return SourceKind::Known(number_literal_kind(raw));
            }
            match bound {
                BoundExpr::ColumnRef { index } => match column_kind(schema, *index) {
                    Some(k) => SourceKind::Known(k),
                    None => SourceKind::UnknownNumeric,
                },
                // 配列の添字の結果は要素型の種別（TABLE-35・Issue #1602）。
                BoundExpr::ArrayIndex { elem, .. } => match elem_cast_type(*elem) {
                    Some(t) => SourceKind::Known(t.kind()),
                    None => SourceKind::UnknownNumeric,
                },
                BoundExpr::Builtin {
                    f: BuiltinFn::TypedArith(_, kind),
                    ..
                } => SourceKind::Known(match kind {
                    NumericKind::Int2 => CastKind::Int2,
                    NumericKind::Int4 => CastKind::Int4,
                    NumericKind::Int8 => CastKind::Int8,
                    NumericKind::Float4 => CastKind::Float4,
                }),
                _ => SourceKind::UnknownNumeric,
            }
        }
        ExprType::Text => match bound {
            BoundExpr::TextColumnRef { index } => {
                SourceKind::Known(column_kind(schema, *index).unwrap_or(CastKind::Text))
            }
            BoundExpr::Builtin {
                f: BuiltinFn::BpcharToText,
                ..
            } => SourceKind::Known(CastKind::Bpchar),
            _ => SourceKind::Known(CastKind::Text),
        },
        ExprType::Bool => SourceKind::Known(CastKind::Bool),
        ExprType::Numeric => SourceKind::Known(CastKind::Numeric),
        ExprType::Date => SourceKind::Known(CastKind::Date),
        ExprType::Timestamp => SourceKind::Known(CastKind::Timestamp),
        ExprType::Time => SourceKind::Known(CastKind::Time),
        ExprType::TimestampTz => SourceKind::Known(CastKind::TimestampTz),
        ExprType::Interval => SourceKind::Known(CastKind::Interval),
        ExprType::Json => SourceKind::Known(CastKind::Jsonb),
        ExprType::JsonText => SourceKind::Known(CastKind::Json),
        ExprType::Uuid => SourceKind::Known(CastKind::Uuid),
        ExprType::Bytea => SourceKind::Known(CastKind::Bytea),
        // 配列を生成するのは配列列の参照（CAST の被演算子の位置）と CAST の結果（上で処理済み）だけ。
        ExprType::Array => match bound {
            BoundExpr::ArrayColumnRef { index } => {
                SourceKind::Array(schema.and_then(|s| s.columns.get(*index)).and_then(|c| {
                    match &c.ty {
                        ColumnType::Array(at) => elem_cast_type(at.elem()),
                        _ => None,
                    }
                }))
            }
            // 配列コンストラクタ・スライスの結果は束縛結果が持つ要素型（TABLE-35・Issue #1602）。
            BoundExpr::ArrayConstructor { elem, .. } | BoundExpr::ArraySlice { elem, .. } => {
                SourceKind::Array(elem_cast_type(*elem))
            }
            _ => SourceKind::Array(None),
        },
        ExprType::Vector => SourceKind::Outside,
    }
}

/// 数値リテラルの型（PostgreSQL と同じく、整数形は int4 → int8 → numeric、小数は numeric）。
pub(super) fn number_literal_kind(raw: &str) -> CastKind {
    match crate::sql::udf_call::classify_integer_literal(raw) {
        Some((_, NumericKind::Int4)) => CastKind::Int4,
        Some((_, NumericKind::Int8)) => CastKind::Int8,
        _ => CastKind::Numeric,
    }
}

fn not_implemented(from: &str, to: &CastType) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: format!("cast from {from} to {} is not supported yet", to.pg_label()),
    }
}

/// numeric の型修飾子 `(precision, scale)` を検査して返す。PostgreSQL 17 は精度 `1..=1000`・
/// 位取り `-1000..=1000` を受理する。精度 0・精度または位取りの上限超過は PostgreSQL と同じ
/// `22023`。PostgreSQL では有効でも本エンジンの `Decimal`（最大 38 桁・scale は精度以下の
/// 非負）で表せない指定は、`22023` の誤報を避けて `0A000`（fail-closed）にする。
fn numeric_typmod(precision: u32, scale: i64) -> Result<(u8, u8), SqlSurfaceError> {
    if precision == 0 || precision > PG_NUMERIC_MAX_PRECISION {
        return Err(SqlSurfaceError::InvalidParameterValue {
            detail: "NUMERIC precision must be between 1 and 1000",
        });
    }
    if !(-i64::from(PG_NUMERIC_MAX_PRECISION)..=i64::from(PG_NUMERIC_MAX_PRECISION))
        .contains(&scale)
    {
        return Err(SqlSurfaceError::InvalidParameterValue {
            detail: "NUMERIC scale must be between -1000 and 1000",
        });
    }
    // PostgreSQL では有効な負の位取り（-1000 以上）は、本エンジンの非負 scale で表せないため未対応。
    if scale < 0 {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "negative numeric scale is not supported".to_string(),
        });
    }
    match (u8::try_from(precision), u8::try_from(scale)) {
        (Ok(p), Ok(sc)) if p <= crate::numeric::MAX_PRECISION && sc <= p => Ok((p, sc)),
        _ => Err(SqlSurfaceError::FeatureNotSupported {
            detail: "numeric precision above 38 or scale above precision is not supported"
                .to_string(),
        }),
    }
}

/// 先頭ゼロ・小数・符号なしの 10 進整数トークンを `u32` として読む（u32 をあふれる桁数は
/// 上限超過として `u32::MAX`）。形が合わなければ `None`（従来の型名解析へ回す）。
fn strict_u32_token(t: Option<&crate::sql::lexer::Token>) -> Option<u32> {
    match t {
        Some(crate::sql::lexer::Token::Number(n))
            if !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
                && !(n.len() > 1 && n.starts_with('0')) =>
        {
            Some(n.parse::<u32>().unwrap_or(u32::MAX))
        }
        _ => None,
    }
}

/// `CAST`／`<type> '<literal>'` の型名を読む（`sql::allowlist::Parser` の
/// `parse_cast_target_type`・`peek_typed_string_literal` が共有する唯一の入口）。
///
/// DDL の型名解析（`parse_column_type_name`。修飾子なしの `numeric`・`numeric(p)` は `42601`）は
/// 変えず、CAST の型名に限り PostgreSQL と同じく修飾子なしの `numeric`／`decimal` と
/// `numeric(p)`（`= numeric(p,0)`）を受け付ける。それ以外の型名・配列は従来の解析と
/// [`CastType::from_sql_type_name`] に任せる。
pub(crate) fn parse_cast_type(
    tokens: &[crate::sql::lexer::Token],
    pos: &mut usize,
) -> Result<CastType, SqlSurfaceError> {
    use crate::sql::lexer::Token;
    let is_numeric_head = matches!(
        tokens.get(*pos),
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("numeric") || w.eq_ignore_ascii_case("decimal")
    );
    if is_numeric_head {
        let is_punct =
            |p: usize, c: char| matches!(tokens.get(p), Some(Token::Punct(x)) if *x == c);
        let array_follows = |p: usize| is_punct(p, '[');
        if !is_punct(*pos + 1, '(') && !array_follows(*pos + 1) {
            // 修飾子なし。
            *pos += 1;
            return Ok(CastType::Numeric(None));
        }
        if is_punct(*pos + 1, '(') {
            // `numeric(p)`。
            if let Some(p) = strict_u32_token(tokens.get(*pos + 2)) {
                if is_punct(*pos + 3, ')') && !array_follows(*pos + 4) {
                    let typmod = numeric_typmod(p, 0)?;
                    *pos += 4;
                    return Ok(CastType::Numeric(Some(typmod)));
                }
                // `numeric(p,s)`。p・s が u8 を超える指定も PostgreSQL では有効な形のため、
                // 構文違反（`42601`）ではなく上限の判定（`22023`／`0A000`）に揃える。
                // 位取りは負も受け付ける（`-` ＋ 数値トークン）。
                if is_punct(*pos + 3, ',') {
                    let negative = is_punct(*pos + 4, '-');
                    let sc_at = *pos + 4 + usize::from(negative);
                    if is_punct(sc_at + 1, ')') && !array_follows(sc_at + 2) {
                        if let Some(sc) = strict_u32_token(tokens.get(sc_at)) {
                            let sc = if negative {
                                -i64::from(sc)
                            } else {
                                i64::from(sc)
                            };
                            let typmod = numeric_typmod(p, sc)?;
                            *pos = sc_at + 2;
                            return Ok(CastType::Numeric(Some(typmod)));
                        }
                    }
                }
            }
        }
    }
    let name = crate::sql::ddl_column_type::parse_column_type_name(tokens, pos)?;
    CastType::from_sql_type_name(&name)
}

/// 規則表の見出しと式の静的型から、値の変換の変換元を決める。式の型が見出しと食い違う組と、
/// 要素型を確定できない配列は `None`。
fn cast_src(kind: CastKind, ty: ExprType, array_elem: Option<ArrayElemType>) -> Option<CastSrc> {
    match (kind, ty) {
        (CastKind::Date, ExprType::Date) => Some(CastSrc::Date),
        (CastKind::Time, ExprType::Time) => Some(CastSrc::Time),
        (CastKind::Timestamp, ExprType::Timestamp) => Some(CastSrc::Timestamp),
        (CastKind::TimestampTz, ExprType::TimestampTz) => Some(CastSrc::TimestampTz),
        (CastKind::Interval, ExprType::Interval) => Some(CastSrc::Interval),
        (CastKind::Json, ExprType::JsonText) => Some(CastSrc::Json),
        (CastKind::Jsonb, ExprType::Json) => Some(CastSrc::Jsonb),
        (CastKind::Uuid, ExprType::Uuid) => Some(CastSrc::Uuid),
        (CastKind::Bytea, ExprType::Bytea) => Some(CastSrc::Bytea),
        (CastKind::Array, ExprType::Array) => array_elem.map(CastSrc::Array),
        (CastKind::Int2, ExprType::Scalar) => Some(CastSrc::Int2),
        (CastKind::Int4, ExprType::Scalar) => Some(CastSrc::Int4),
        (CastKind::Int8, ExprType::Scalar) => Some(CastSrc::Int8),
        (CastKind::Float4, ExprType::Scalar) => Some(CastSrc::Float4),
        (CastKind::Float8, ExprType::Scalar) => Some(CastSrc::Float8),
        (CastKind::Numeric, ExprType::Numeric) => Some(CastSrc::Numeric),
        (CastKind::Bool, ExprType::Bool) => Some(CastSrc::Bool),
        (CastKind::Text | CastKind::Varchar | CastKind::Bpchar, ExprType::Text) => {
            Some(CastSrc::Text)
        }
        _ => None,
    }
}

/// 変換先の型から値の変換の変換先を決める（対象外の型は `None`）。
fn cast_dst(target: &CastType) -> Option<CastDst> {
    match target {
        CastType::Int2 => Some(CastDst::Int2),
        CastType::Int4 => Some(CastDst::Int4),
        CastType::Int8 => Some(CastDst::Int8),
        CastType::Float4 => Some(CastDst::Float4),
        CastType::Float8 => Some(CastDst::Float8),
        CastType::Numeric(typmod) => Some(CastDst::Numeric(*typmod)),
        CastType::Bool => Some(CastDst::Bool),
        CastType::Text => Some(CastDst::Text),
        CastType::Varchar(n) => Some(CastDst::Varchar(*n)),
        CastType::Bpchar(n) => Some(CastDst::Bpchar(*n)),
        CastType::Date => Some(CastDst::Date),
        CastType::Time => Some(CastDst::Time),
        CastType::Timestamp => Some(CastDst::Timestamp),
        CastType::TimestampTz => Some(CastDst::TimestampTz),
        CastType::Interval => Some(CastDst::Interval),
        CastType::Json => Some(CastDst::Json),
        CastType::Jsonb => Some(CastDst::Jsonb),
        CastType::Uuid => Some(CastDst::Uuid),
        CastType::Bytea => Some(CastDst::Bytea),
        CastType::Array(_) => target.array_elem_type().map(CastDst::Array),
    }
}

/// 変換ノードを組み立てる（引数 1 個の `BuiltinFn::Cast`）。
fn cast_node(f: CastFn, arg: BoundExpr) -> BoundExpr {
    BoundExpr::Builtin {
        f: BuiltinFn::Cast(f),
        args: vec![arg],
    }
}

/// 型なし文字列リテラルを変換先の型の入力として解釈する束縛。対応する変換先は
/// date／time／timestamp／timestamptz／interval（既存の型付きリテラルと同じ解析）・jsonb・
/// 数値 6 型・boolean・文字列 3 型（Issue #1591）・json／uuid／bytea／配列（Issue #1592。
/// 対応する要素型の無い配列は未実装の `0A000`）。
///
/// 数値・boolean・型修飾子つき文字列・json・uuid・bytea・配列は、束縛時に実行時と同じ入力関数
/// （`sql::cast_value::apply_cast`）を 1 度呼んで形式不正・範囲外を早期に報告し、評価は
/// 変換ノードに委ねる（第 2 の文法を作らない）。CHECK 本体では値の変換を持ち込まない。
fn bind_string_literal_as(
    s: &str,
    target: &CastType,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    use crate::sql::cast_value::{datetime_literal_error as datetime_err, interval_literal_error};
    let literal = match target {
        CastType::Date => Expr::DateLiteral(crate::datetime::parse_date(s).map_err(datetime_err)?),
        CastType::Time => Expr::TimeLiteral(crate::datetime::parse_time(s).map_err(datetime_err)?),
        CastType::Timestamp => {
            Expr::TimestampLiteral(crate::datetime::parse_timestamp(s).map_err(datetime_err)?)
        }
        CastType::TimestampTz => crate::sql::allowlist::parse_timestamptz_expr_literal(s)?,
        CastType::Interval => Expr::IntervalLiteral(
            crate::datetime::parse_interval(s).map_err(interval_literal_error)?,
        ),
        CastType::Jsonb => {
            // 文字列リテラルのノード 1 個分を課金する（`bind_expr_any` を通らないため）。
            charge_node(node_budget)?;
            return bind_jsonb_literal(s);
        }
        CastType::Text | CastType::Varchar(None) => {
            return bind_expr_any(&Expr::String(s.to_string()), env, node_budget)
        }
        CastType::Int2
        | CastType::Int4
        | CastType::Int8
        | CastType::Float4
        | CastType::Float8
        | CastType::Numeric(_)
        | CastType::Bool
        | CastType::Varchar(Some(_))
        | CastType::Bpchar(_)
        | CastType::Json
        | CastType::Uuid
        | CastType::Bytea
        | CastType::Array(_) => {
            let (Some(dst), Some(ty)) = (cast_dst(target), target.expr_type()) else {
                return Err(not_implemented("unknown", target));
            };
            if crate::sql::udf_call::in_check_binding() {
                return Err(not_implemented("unknown", target));
            }
            if s.len() > crate::row_codec::MAX_TEXT_FIELD_LEN as usize {
                return Err(SqlSurfaceError::payload_too_large(
                    "string literal exceeds the maximum TEXT field length",
                ));
            }
            // 文字列リテラルと変換ノードの 2 ノード分を課金する（`bind_expr_any` を通らないため）。
            charge_node(node_budget)?;
            charge_node(node_budget)?;
            let f = CastFn {
                from: CastSrc::Text,
                to: dst,
            };
            // 早期検証（形式不正・範囲外・未対応の非有限値を束縛時に報告する）。
            crate::sql::cast_value::apply_cast(
                f,
                crate::sql::udf_call::ExprValue::Text(std::borrow::Cow::Borrowed(s)),
            )?;
            return Ok((cast_node(f, BoundExpr::Text(s.to_string())), ty));
        }
    };
    // 解析済みの定数ノードを通常の束縛へ流す（TIMESTAMPTZ のセッション TZ 確定もここで行う）。
    bind_expr_any(&literal, env, node_budget)
}

pub(super) fn charge_node(node_budget: &mut usize) -> Result<(), SqlSurfaceError> {
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    Ok(())
}

/// 2^53（式評価が整数を f64 で正確に運べる上限）を超える整数形リテラルか。
pub(super) fn exceeds_exact_integer(raw: &str) -> bool {
    match crate::sql::udf_call::classify_integer_literal(raw) {
        Some((v, _)) => v.unsigned_abs() > (1u64 << 53),
        None => false,
    }
}

/// `CAST` の被演算子になる NUMERIC／BOOLEAN／UUID／BYTEA／配列列の参照を、CAST の位置でだけ受理して葉にする。
/// それ以外の位置の参照は従来どおり束縛段が拒否する。UDF 本体（スキーマなし）は対象外。
pub(super) fn bind_cast_operand_column(
    name: &str,
    env: &BindEnv<'_>,
) -> Option<(BoundExpr, ExprType)> {
    let schema = env.schema()?;
    // UDF の仮引数は列より優先される（`Expr::Ident` の通常の束縛順序と同じ）。
    if env.has_param(name) {
        return None;
    }
    let (index, column) = schema
        .columns
        .iter()
        .enumerate()
        .find(|(_, c)| c.name == name)?;
    match column.ty {
        ColumnType::Numeric { .. } => {
            Some((BoundExpr::DecimalColumnRef { index }, ExprType::Numeric))
        }
        ColumnType::Boolean => Some((BoundExpr::BoolColumnRef { index }, ExprType::Bool)),
        ColumnType::Uuid => Some((BoundExpr::UuidColumnRef { index }, ExprType::Uuid)),
        ColumnType::Bytea => Some((BoundExpr::ByteaColumnRef { index }, ExprType::Bytea)),
        ColumnType::Array(_) => Some((BoundExpr::ArrayColumnRef { index }, ExprType::Array)),
        _ => None,
    }
}

/// 配列値の式を配列列へ代入するための変換を作る（`UPDATE ... SET <配列列> = <配列式>`。
/// TABLE-35・Issue #1602。呼び出し元は `sql::set_expr::bind_set_expr`）。
///
/// 要素型が列と同じなら式をそのまま返す。異なる場合は PG の代入変換と同じく、要素の規則が
/// 恒等・暗黙・代入・文字列型への I/O 代入のいずれかのときだけ `BuiltinFn::Cast`（配列 → 配列）を
/// 挟み、それ以外は `42804`。要素型を静的に確定できない式・ENUM 要素の列は `0A000`。
/// `NUMERIC(p,s)[]` 列へは位取りの異なる要素型の組として必ず変換ノードを通り、列の位取りへ
/// 正規化される。
pub(super) fn bind_array_assignment(
    expr: &Expr,
    bound: BoundExpr,
    schema: &TableSchema,
    target: &ArrayType,
) -> Result<BoundExpr, SqlSurfaceError> {
    let unsupported = |detail: &str| SqlSurfaceError::FeatureNotSupported {
        detail: detail.to_string(),
    };
    let Some(tgt_ty) = elem_cast_type(target.elem()) else {
        return Err(unsupported(
            "assigning an array expression to this column type is not supported",
        ));
    };
    let src_ty = match source_cast_kind(expr, &bound, ExprType::Array, Some(schema)) {
        SourceKind::Array(Some(t)) => t,
        _ => {
            return Err(unsupported(
                "the element type of this array expression cannot be determined",
            ))
        }
    };
    if src_ty == tgt_ty {
        return Ok(bound);
    }
    if !matches!(
        cast_rule(src_ty.kind(), tgt_ty.kind()),
        CastRule::Identity | CastRule::Implicit | CastRule::Assignment | CastRule::IoAssignment
    ) {
        return Err(SqlSurfaceError::datatype_mismatch(
            "column cannot be assigned an array with a different element type",
        ));
    }
    let src_arr = CastType::Array(Box::new(src_ty));
    let tgt_arr = CastType::Array(Box::new(tgt_ty));
    let (Some(src), Some(dst)) = (
        cast_src(CastKind::Array, ExprType::Array, src_arr.array_elem_type()),
        cast_dst(&tgt_arr),
    ) else {
        return Err(unsupported(
            "assigning an array expression to this column type is not supported",
        ));
    };
    Ok(cast_node(CastFn { from: src, to: dst }, bound))
}

/// 配列の要素・添字の基底など、CAST の被演算子と同じ規則で式を束縛する（列参照は numeric／bool／
/// uuid／bytea／配列の列も葉として受理する。TABLE-35・Issue #1602 の `sql::array_expr` が使う）。
pub(super) fn bind_operand(
    operand: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if let Expr::Ident(name) = operand {
        if let Some(leaf) = bind_cast_operand_column(name, env) {
            charge_node(node_budget)?;
            return Ok(leaf);
        }
    }
    bind_expr_any(operand, env, node_budget)
}

/// `CAST(<operand> AS <target>)` の束縛（`udf_call::bind_expr_any` の `Expr::Cast` 腕から呼ばれる）。
///
/// 束縛の結果は次のいずれか。
/// - `42846`: 規則表で `×` の組（型の組だけで決まり、値が NULL でも同じ）。
/// - 恒等（同一型で型修飾子なし）や値を変えない拡幅: 内側の束縛結果をそのまま返す。
/// - 値の変換（数値 6 型・BOOLEAN・文字列 3 型・日時・JSON・UUID・BYTEA・配列。Issue #1591・#1592）:
///   `BuiltinFn::Cast`。CHECK 本体では値の変換を持ち込まず `0A000`（全テナントを走査する検証経路を避ける）。
/// - 型なし文字列リテラルを入力として解釈、または型付き NULL。
/// - それ以外（対応する要素型の無い配列・種別不明の数値）は `0A000`。
pub(super) fn bind_cast(
    operand: &Expr,
    target: &CastType,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let target_ty = target.expr_type();
    // 空の `ARRAY[]` は変換先の要素型の空配列にする（`ARRAY[]::int[]`。PG と同じく型は変換先で決まる）。
    if let (Expr::ArrayConstructor(items), CastType::Array(_)) = (strip_collate(operand), target) {
        if items.is_empty() {
            charge_node(node_budget)?;
            return Ok((
                crate::sql::array_expr::bind_empty_array_as(target)?,
                ExprType::Array,
            ));
        }
    }
    let (bound, ty) = match operand {
        // 型付き NULL（`CAST(NULL AS integer)`）。変換先に式の型が無ければ未実装。
        Expr::Null => {
            charge_node(node_budget)?;
            return match target_ty {
                Some(ty) => Ok((BoundExpr::Null, ty)),
                None => Err(not_implemented("unknown", target)),
            };
        }
        Expr::String(s) => return bind_string_literal_as(s, target, env, node_budget),
        // 小数形・2^53 を超える整数形のリテラルは、f64 を経由せず正確な numeric 定数にする
        // （f64 では numeric の丸め〔`2.5` → 3〕が狂う）。
        Expr::Number(raw)
            if number_literal_kind(raw) == CastKind::Numeric || exceeds_exact_integer(raw) =>
        {
            charge_node(node_budget)?;
            let d = crate::numeric::parse_literal_exact(raw).map_err(|_| {
                SqlSurfaceError::numeric_out_of_range("value overflows numeric format")
            })?;
            (BoundExpr::Numeric(d), ExprType::Numeric)
        }
        Expr::Ident(name) => match bind_cast_operand_column(name, env) {
            Some(leaf) => {
                charge_node(node_budget)?;
                leaf
            }
            None => bind_expr_any(operand, env, node_budget)?,
        },
        _ => bind_expr_any(operand, env, node_budget)?,
    };
    cast_bound(operand, bound, ty, target, env, node_budget)
}

/// 束縛済みの被演算子 `bound`（式の型 `ty`。AST は `operand`）を `target` へ変換する
/// [`bind_cast`] の後半。配列コンストラクタ・添字（`sql::array_expr`）が、要素・添字の式を
/// 分類のために 1 度束縛した結果を再束縛せずに変換へ渡すために公開する（入れ子の式を束縛し直すと
/// 入れ子の深さに対して指数的に束縛が重複するため。TABLE-35・Issue #1602）。
pub(super) fn cast_bound(
    operand: &Expr,
    bound: BoundExpr,
    ty: ExprType,
    target: &CastType,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let target_ty = target.expr_type();
    let source = source_cast_kind(operand, &bound, ty, env.schema());
    // 配列は要素型つきで規則を引く（`Some(None)` は要素型を CAST の型で表せない ENUM 配列）。
    let mut array_from: Option<Option<CastType>> = None;
    let from_kind = match source {
        SourceKind::Known(k) => k,
        SourceKind::Array(elem) => {
            array_from = Some(elem);
            CastKind::Array
        }
        SourceKind::Outside => return Err(not_implemented("vector", target)),
        SourceKind::UnknownNumeric => {
            // 数値の行どうしは変換先によって規則が異なる（numeric と float で丸め・表記が異なる）。
            // 数値 6 型・bool・文字列 3 型では誤った値・`42846`・恒等を作らないよう未実装とする。
            // 他の変換先では全数値行が同じ規則。
            if matches!(
                target.kind(),
                CastKind::Int4
                    | CastKind::Int8
                    | CastKind::Int2
                    | CastKind::Float4
                    | CastKind::Float8
                    | CastKind::Numeric
                    | CastKind::Bool
                    | CastKind::Text
                    | CastKind::Varchar
                    | CastKind::Bpchar
            ) {
                return Err(not_implemented("numeric", target));
            }
            CastKind::Int4
        }
    };
    // 配列 → 配列は要素型の組で規則を引き直す。変換元の要素型を確定できない（ENUM 要素の）配列は、
    // 配列への変換を未実装（`0A000`）とする（文字列型への変換と `×` の判定は種別だけで足りる）。
    let source_array_ty = match &array_from {
        Some(Some(elem)) => Some(CastType::Array(Box::new(elem.clone()))),
        _ => None,
    };
    let rule = match (&source_array_ty, target) {
        (Some(from_ty), CastType::Array(_)) => cast_rule_types(from_ty, target),
        (None, CastType::Array(_)) if array_from.is_some() => {
            return Err(not_implemented("enum[]", target))
        }
        _ => cast_rule(from_kind, target.kind()),
    };
    if rule == CastRule::None {
        return Err(SqlSurfaceError::CannotCoerce {
            from: match &source_array_ty {
                Some(from_ty) => from_ty.pg_label(),
                None => from_kind.label().to_string(),
            },
            to: target.pg_label(),
        });
    }
    // 同じ要素型の配列どうしは値を変えない（修飾子つきの要素型は `array_elem_type` が拒否済み）。
    let same_array = source_array_ty.as_ref() == Some(target);
    if ((rule == CastRule::Identity && !target.has_modifier()) || same_array)
        && target_ty == Some(ty)
    {
        return Ok((bound, ty));
    }
    let source_elem = match (&source_array_ty, &array_from) {
        (Some(from_ty), _) => from_ty.array_elem_type(),
        // 要素型を CAST の型で表せない（ENUM 要素の）配列。配列への変換は上で `0A000` にしており、
        // ここへ来るのは文字列型への変換だけ（描画は要素型に依らない）。
        (None, Some(None)) => Some(ArrayElemType::Enum),
        _ => None,
    };
    let (Some(src), Some(dst), Some(result_ty)) = (
        cast_src(from_kind, ty, source_elem),
        cast_dst(target),
        target_ty,
    ) else {
        return Err(not_implemented(from_kind.label(), target));
    };
    let f = CastFn { from: src, to: dst };
    // CHAR(n) への変換結果（末尾空白で埋めた値）は、別の型へ変換する前に bpchar → text と同じく
    // 末尾の半角空白を除く（列参照は束縛時に除去済み）。
    let inner_is_bpchar = matches!(
        strip_collate(operand),
        Expr::Cast { ty: inner, .. } if matches!(inner, CastType::Bpchar(_))
    );
    let bound = if inner_is_bpchar && src == CastSrc::Text {
        BoundExpr::Builtin {
            f: BuiltinFn::BpcharToText,
            args: vec![bound],
        }
    } else {
        bound
    };
    if f.is_noop() && result_ty == ty {
        return Ok((bound, ty));
    }
    if crate::sql::udf_call::in_check_binding() {
        return Err(not_implemented(from_kind.label(), target));
    }
    charge_node(node_budget)?;
    Ok((cast_node(f, bound), result_ty))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    /// 設計書の表（`docs/design/cast-conversion-rules.md`）を 20 行の記号列として持つ。
    /// 行・列の並びは `CastKind` の宣言順。
    const DOC_TABLE: [(&str, &str); 20] = [
        ("int4", "= i a i i i e io/a io/a io/a x x x x x x x x x x"),
        ("int8", "a = a i i i x io/a io/a io/a x x x x x x x x x x"),
        ("int2", "i i = i i i x io/a io/a io/a x x x x x x x x x x"),
        ("float4", "a a a = i a x io/a io/a io/a x x x x x x x x x x"),
        ("float8", "a a a a = a x io/a io/a io/a x x x x x x x x x x"),
        ("numeric", "a a a i i = x io/a io/a io/a x x x x x x x x x x"),
        ("bool", "e x x x x x = a a a x x x x x x x x x x"),
        ("text", "io/e io/e io/e io/e io/e io/e io/e = i i io/e io/e io/e io/e io/e io/e io/e io/e io/e io/e"),
        ("varchar", "io/e io/e io/e io/e io/e io/e io/e i = i io/e io/e io/e io/e io/e io/e io/e io/e io/e io/e"),
        ("bpchar", "io/e io/e io/e io/e io/e io/e io/e i i = io/e io/e io/e io/e io/e io/e io/e io/e io/e io/e"),
        ("date", "x x x x x x x io/a io/a io/a = x i i x x x x x x"),
        ("time", "x x x x x x x io/a io/a io/a x = x x i x x x x x"),
        ("timestamp", "x x x x x x x io/a io/a io/a a a = i x x x x x x"),
        ("timestamptz", "x x x x x x x io/a io/a io/a a a a = x x x x x x"),
        ("interval", "x x x x x x x io/a io/a io/a x a x x = x x x x x"),
        ("json", "x x x x x x x io/a io/a io/a x x x x x = a x x x"),
        ("jsonb", "e e e e e e e io/a io/a io/a x x x x x a = x x x"),
        ("uuid", "x x x x x x x io/a io/a io/a x x x x x x x = x x"),
        ("bytea", "x x x x x x x io/a io/a io/a x x x x x x x x = x"),
        ("array", "x x x x x x x io/a io/a io/a x x x x x x x x x elem"),
    ];

    const KINDS: [(&str, CastKind); 20] = [
        ("int4", CastKind::Int4),
        ("int8", CastKind::Int8),
        ("int2", CastKind::Int2),
        ("float4", CastKind::Float4),
        ("float8", CastKind::Float8),
        ("numeric", CastKind::Numeric),
        ("bool", CastKind::Bool),
        ("text", CastKind::Text),
        ("varchar", CastKind::Varchar),
        ("bpchar", CastKind::Bpchar),
        ("date", CastKind::Date),
        ("time", CastKind::Time),
        ("timestamp", CastKind::Timestamp),
        ("timestamptz", CastKind::TimestampTz),
        ("interval", CastKind::Interval),
        ("json", CastKind::Json),
        ("jsonb", CastKind::Jsonb),
        ("uuid", CastKind::Uuid),
        ("bytea", CastKind::Bytea),
        ("array", CastKind::Array),
    ];

    fn symbol(rule: CastRule) -> &'static str {
        match rule {
            CastRule::Identity => "=",
            CastRule::Implicit => "i",
            CastRule::Assignment => "a",
            CastRule::Explicit => "e",
            CastRule::IoAssignment => "io/a",
            CastRule::IoExplicit => "io/e",
            CastRule::Element => "elem",
            CastRule::None => "x",
        }
    }

    /// 全 400 セルを設計書の表と照合する。失敗時はセルキー `<変換元>-><変換先>` を出す。
    #[test]
    fn rule_table_matches_design_doc_for_all_400_cells() {
        let mut checked = 0;
        for (row_name, cells) in DOC_TABLE {
            let from = KINDS
                .iter()
                .find(|(n, _)| *n == row_name)
                .map(|(_, k)| *k)
                .expect("row kind");
            let symbols: Vec<&str> = cells.split(' ').collect();
            assert_eq!(symbols.len(), 20, "row {row_name}");
            for (col, sym) in symbols.iter().enumerate() {
                let (col_name, to) = KINDS[col];
                assert_eq!(
                    symbol(cast_rule(from, to)),
                    *sym,
                    "cell {row_name}->{col_name}"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 400);
    }

    #[test]
    fn array_to_array_follows_element_rule() {
        let arr = |e: CastType| CastType::Array(Box::new(e));
        assert_eq!(
            cast_rule_types(&arr(CastType::Int4), &arr(CastType::Text)),
            CastRule::Element
        );
        assert_eq!(
            cast_rule_types(&arr(CastType::Int4), &arr(CastType::Uuid)),
            CastRule::None
        );
        assert_eq!(
            cast_rule_types(&arr(CastType::Int4), &CastType::Int4),
            CastRule::None
        );
    }

    fn parse_ty(sql: &str) -> Result<CastType, SqlSurfaceError> {
        let tokens = tokenize(sql).expect("tokenize");
        let mut pos = 0;
        let ty = parse_cast_type(&tokens, &mut pos)?;
        assert_eq!(pos, tokens.len(), "all tokens consumed for {sql}");
        Ok(ty)
    }

    #[test]
    fn aliases_resolve_to_builtin_types() {
        for (sql, want) in [
            ("int4", CastType::Int4),
            ("INT8", CastType::Int8),
            ("int2", CastType::Int2),
            ("float4", CastType::Float4),
            ("float8", CastType::Float8),
            ("varchar", CastType::Varchar(None)),
            ("character varying(3)", CastType::Varchar(Some(3))),
            ("char(2)", CastType::Bpchar(2)),
            ("time", CastType::Time),
            ("timestamptz", CastType::TimestampTz),
            ("timestamp without time zone", CastType::Timestamp),
            ("double precision", CastType::Float8),
            ("bool", CastType::Bool),
        ] {
            assert_eq!(parse_ty(sql).expect(sql), want, "{sql}");
        }
    }

    #[test]
    fn enum_candidate_is_undefined_type_and_vector_is_unsupported() {
        assert!(matches!(
            parse_ty("nosuchtype"),
            Err(SqlSurfaceError::UndefinedType { .. })
        ));
        assert!(matches!(
            parse_ty("vector(3)"),
            Err(SqlSurfaceError::FeatureNotSupported { .. })
        ));
    }

    #[test]
    fn array_type_drops_length_and_wraps_element() {
        assert_eq!(
            parse_ty("int4[5]").expect("array"),
            CastType::Array(Box::new(CastType::Int4))
        );
    }

    #[test]
    fn to_sql_round_trips_through_the_type_name_parser() {
        let all = [
            CastType::Int2,
            CastType::Int4,
            CastType::Int8,
            CastType::Float4,
            CastType::Float8,
            CastType::Numeric(Some((10, 2))),
            CastType::Numeric(None),
            CastType::Bool,
            CastType::Text,
            CastType::Varchar(None),
            CastType::Varchar(Some(7)),
            CastType::Bpchar(3),
            CastType::Date,
            CastType::Time,
            CastType::Timestamp,
            CastType::TimestampTz,
            CastType::Interval,
            CastType::Json,
            CastType::Jsonb,
            CastType::Uuid,
            CastType::Bytea,
            CastType::Array(Box::new(CastType::Int4)),
            CastType::Array(Box::new(CastType::Varchar(Some(3)))),
        ];
        for ty in all {
            let sql = ty.to_sql();
            assert_eq!(parse_ty(&sql).expect(&sql), ty, "{sql}");
        }
    }

    /// `to_sql` の `(p,s)` 形はバイト列が content_hash（tag 16）と台帳の再送に使われるため不変。
    #[test]
    fn numeric_to_sql_bytes_are_stable() {
        assert_eq!(CastType::Numeric(Some((10, 2))).to_sql(), "numeric(10,2)");
        assert_eq!(CastType::Numeric(Some((38, 0))).to_sql(), "numeric(38,0)");
        assert_eq!(CastType::Numeric(None).to_sql(), "numeric");
    }

    #[test]
    fn cast_numeric_type_name_forms() {
        assert_eq!(parse_ty("numeric").expect("bare"), CastType::Numeric(None));
        assert_eq!(parse_ty("DECIMAL").expect("bare"), CastType::Numeric(None));
        assert_eq!(
            parse_ty("numeric(7)").expect("p"),
            CastType::Numeric(Some((7, 0)))
        );
        assert_eq!(
            parse_ty("numeric(7, 3)").expect("ps"),
            CastType::Numeric(Some((7, 3)))
        );
        // PostgreSQL では無効な精度は 22023、有効だが本エンジンで表せない指定は 0A000。
        for bad in [
            "numeric(0)",
            "numeric(0,0)",
            "numeric(1001)",
            "numeric(5000,1)",
            "numeric(5,1001)",
            "numeric(5,-1001)",
            "numeric(5,-99999999999)",
        ] {
            assert!(
                matches!(
                    parse_ty(bad),
                    Err(SqlSurfaceError::InvalidParameterValue { .. })
                ),
                "{bad}"
            );
        }
        for unsupported in [
            "numeric(39)",
            "numeric(50,2)",
            "numeric(3,5)",
            "numeric(1000,0)",
            "numeric(5,-1)",
            "numeric(5, -1000)",
        ] {
            assert!(
                matches!(
                    parse_ty(unsupported),
                    Err(SqlSurfaceError::FeatureNotSupported { .. })
                ),
                "{unsupported}"
            );
        }
    }

    #[test]
    fn pg_label_is_static_and_unqualified() {
        assert_eq!(CastType::Varchar(Some(3)).pg_label(), "character varying");
        assert_eq!(CastType::Time.pg_label(), "time without time zone");
        assert_eq!(
            CastType::Array(Box::new(CastType::Int4)).pg_label(),
            "integer[]"
        );
    }

    #[test]
    fn expr_type_mapping() {
        assert_eq!(CastType::Int8.expr_type(), Some(ExprType::Scalar));
        assert_eq!(CastType::Jsonb.expr_type(), Some(ExprType::Json));
        assert_eq!(CastType::Json.expr_type(), Some(ExprType::JsonText));
        assert_eq!(CastType::Bpchar(2).expr_type(), Some(ExprType::Text));
        assert_eq!(CastType::Numeric(None).expr_type(), Some(ExprType::Numeric));
        assert_eq!(CastType::Uuid.expr_type(), Some(ExprType::Uuid));
        assert_eq!(CastType::Bytea.expr_type(), Some(ExprType::Bytea));
        let arr = |e: CastType| CastType::Array(Box::new(e));
        assert_eq!(arr(CastType::Int4).expr_type(), Some(ExprType::Array));
        // 対応する要素型の無い配列は式の型を持たない（束縛段が `0A000` にする）。
        assert_eq!(arr(CastType::Int2).expr_type(), None);
        assert_eq!(arr(CastType::Numeric(None)).expr_type(), None);
    }

    #[test]
    fn array_element_types_map_to_row_codec_types() {
        let arr = |e: CastType| CastType::Array(Box::new(e));
        for (elem, want) in [
            (CastType::Text, ArrayElemType::Text),
            (CastType::Bool, ArrayElemType::Bool),
            (CastType::Int4, ArrayElemType::Integer),
            (CastType::Int8, ArrayElemType::BigInt),
            (CastType::Float4, ArrayElemType::Real),
            (CastType::Float8, ArrayElemType::Double),
            (CastType::Date, ArrayElemType::Date),
            (CastType::Timestamp, ArrayElemType::Timestamp),
            (CastType::Uuid, ArrayElemType::Uuid),
            (
                CastType::Numeric(Some((10, 2))),
                ArrayElemType::Numeric {
                    precision: 10,
                    scale: 2,
                },
            ),
            (CastType::Bytea, ArrayElemType::Bytea),
            (CastType::Json, ArrayElemType::Json),
            (CastType::Jsonb, ArrayElemType::Jsonb),
        ] {
            assert_eq!(arr(elem.clone()).array_elem_type(), Some(want), "{elem:?}");
            assert!(arr(elem).result_column_type().is_some());
        }
        for elem in [
            CastType::Int2,
            CastType::Time,
            CastType::TimestampTz,
            CastType::Interval,
            CastType::Varchar(Some(3)),
            CastType::Bpchar(3),
            CastType::Numeric(None),
        ] {
            assert_eq!(arr(elem.clone()).array_elem_type(), None, "{elem:?}");
            assert_eq!(arr(elem).result_column_type(), None);
        }
        assert_eq!(CastType::Int4.array_elem_type(), None);
    }

    #[test]
    fn array_rule_follows_the_element_pair() {
        let arr = |e: CastType| CastType::Array(Box::new(e));
        // 要素の組が `×`（uuid -> 整数）なら配列も `×`、そうでなければ `elem`。
        assert_eq!(
            cast_rule_types(&arr(CastType::Uuid), &arr(CastType::Int4)),
            CastRule::None
        );
        assert_eq!(
            cast_rule_types(&arr(CastType::Int4), &arr(CastType::Int8)),
            CastRule::Element
        );
        assert_eq!(
            cast_rule_types(&arr(CastType::Text), &arr(CastType::Date)),
            CastRule::Element
        );
    }
}
