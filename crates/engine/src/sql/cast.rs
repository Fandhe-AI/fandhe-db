//! `CAST(<expr> AS <type>)` の型・規則表・束縛（Issue #1590・SQL-47 のポインタ）。
//!
//! 責務境界: 構文段（`sql::allowlist::Parser::parse_cast_expr`）が作る [`Expr::Cast`] を、
//! 束縛段（`sql::udf_call::bind_expr_any` の `Expr::Cast` 腕）が [`bind_cast`] で
//! [`BoundExpr`] へ落とす。変換先の型名は DDL と同じ型名解決
//! （`sql::ddl_column_type::parse_column_type_name`）で読み、[`CastType::from_sql_type_name`]
//! で CAST 用の閉じた型集合へ写す（カタログは参照しない静的な判定）。
//!
//! 規則表（20 型 × 20 型）は `docs/design/cast-conversion-rules.md` の表をデータとして
//! 持つ（[`cast_rule`]）。型の組だけで決まる `×`（`42846`）は束縛時に判定し、値の変換
//! （`i`・`a`・`e`・`io/*`・型修飾子の適用）は兄弟 Issue（#1591・#1592）の担当で、本モジュールでは
//! 未実装の組を `0A000` で拒否する（黙って別の型として評価しない。fail-closed）。
//! 判定の順序は「型名の解決（`42704`／`42601`／`0A000`）→ 型の組（`42846`）→ 実装の有無（`0A000`）」。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::sql::udf_call::{
    bind_expr_any, bind_jsonb_literal, BindEnv, BoundExpr, BuiltinFn, Expr, ExprType, NumericKind,
};

/// 変換先（または変換元）の型。CAST が扱う 20 型（配列は要素型つきで 1 種）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastType {
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric {
        precision: u8,
        scale: u8,
    },
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
            CastType::Numeric { .. } => CastKind::Numeric,
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
            SqlColumnTypeName::Numeric { precision, scale } => CastType::Numeric {
                precision: *precision,
                scale: *scale,
            },
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
            CastType::Numeric { precision, scale } => format!("numeric({precision},{scale})"),
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
            CastType::Numeric { .. } => "numeric",
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

    /// 型修飾子（長さ・精度）を持つか。修飾子の適用は値の変換を伴うため、恒等でも
    /// 本 Issue では素通しにしない。
    pub fn has_modifier(&self) -> bool {
        matches!(
            self,
            CastType::Varchar(Some(_)) | CastType::Bpchar(_) | CastType::Numeric { .. }
        )
    }

    /// 束縛段の式の型。対応が無い型（numeric・uuid・bytea・配列）は `None`。
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
            CastType::Numeric { .. } | CastType::Uuid | CastType::Bytea | CastType::Array(_) => {
                None
            }
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
            CastType::Numeric { .. } | CastType::Uuid | CastType::Bytea | CastType::Array(_) => {
                None
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
enum SourceKind {
    /// 規則表の見出しが確定した。
    Known(CastKind),
    /// 数値だが種別（整数・float・numeric）を静的に決められない
    /// （`id` 疑似列・関数・算術・CASE など）。
    UnknownNumeric,
    /// 規則表の外（`vector`）。
    Outside,
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

/// 変換元の静的な型を、束縛済みの式から導く。
///
/// 数値は列参照・整数／小数リテラル・型付き算術・入れ子の CAST から種別を決め、決められないものは
/// [`SourceKind::UnknownNumeric`]。TEXT は列の宣言型（text／varchar）と `BpcharToText` から復元する
/// （三者は `42846` の判定がすべて同じなので、取り違えても `×` の判定は変わらない。恒等の判定にだけ
/// 正確さが要る）。
fn source_cast_kind(
    operand: &Expr,
    bound: &BoundExpr,
    ty: ExprType,
    schema: Option<&TableSchema>,
) -> SourceKind {
    // 入れ子の CAST の結果は、内側の変換先の型（恒等・リテラル・型付き NULL のいずれも）。
    if let Expr::Cast { ty: inner, .. } = operand {
        return SourceKind::Known(inner.kind());
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
        ExprType::Date => SourceKind::Known(CastKind::Date),
        ExprType::Timestamp => SourceKind::Known(CastKind::Timestamp),
        ExprType::Time => SourceKind::Known(CastKind::Time),
        ExprType::TimestampTz => SourceKind::Known(CastKind::TimestampTz),
        ExprType::Interval => SourceKind::Known(CastKind::Interval),
        ExprType::Json => SourceKind::Known(CastKind::Jsonb),
        ExprType::JsonText => SourceKind::Known(CastKind::Json),
        ExprType::Vector => SourceKind::Outside,
    }
}

/// 数値リテラルの型（PostgreSQL と同じく、整数形は int4 → int8 → numeric、小数は numeric）。
fn number_literal_kind(raw: &str) -> CastKind {
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

/// 型なし文字列リテラルを変換先の型の入力として解釈する束縛。対応する変換先は
/// date／time／timestamp／timestamptz／interval（既存の型付きリテラルと同じ解析）・jsonb・
/// 修飾子なしの text／varchar。それ以外は未実装（`0A000`）。
fn bind_string_literal_as(
    s: &str,
    target: &CastType,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    use crate::datetime::{DateTimeLiteralError, IntervalLiteralError};
    let datetime_err = |e: DateTimeLiteralError| match e {
        DateTimeLiteralError::Format(detail) => SqlSurfaceError::invalid_datetime_format(detail),
        DateTimeLiteralError::Overflow(detail) => SqlSurfaceError::datetime_field_overflow(detail),
    };
    let literal = match target {
        CastType::Date => Expr::DateLiteral(crate::datetime::parse_date(s).map_err(datetime_err)?),
        CastType::Time => Expr::TimeLiteral(crate::datetime::parse_time(s).map_err(datetime_err)?),
        CastType::Timestamp => {
            Expr::TimestampLiteral(crate::datetime::parse_timestamp(s).map_err(datetime_err)?)
        }
        CastType::TimestampTz => crate::sql::allowlist::parse_timestamptz_expr_literal(s)?,
        CastType::Interval => match crate::datetime::parse_interval(s) {
            Ok(v) => Expr::IntervalLiteral(v),
            Err(IntervalLiteralError::Format(detail)) => {
                return Err(SqlSurfaceError::invalid_datetime_format(detail))
            }
            Err(e @ IntervalLiteralError::FieldOverflow) => {
                return Err(SqlSurfaceError::interval_field_overflow(e.to_string()))
            }
            Err(e @ IntervalLiteralError::OutOfRange) => {
                return Err(SqlSurfaceError::datetime_field_overflow(e.to_string()))
            }
            Err(IntervalLiteralError::Infinity) => {
                return Err(SqlSurfaceError::FeatureNotSupported {
                    detail: "infinite interval values are not supported yet".to_string(),
                })
            }
        },
        CastType::Jsonb => {
            // 文字列リテラルのノード 1 個分を課金する（`bind_expr_any` を通らないため）。
            charge_node(node_budget)?;
            return bind_jsonb_literal(s);
        }
        CastType::Text | CastType::Varchar(None) => {
            return bind_expr_any(&Expr::String(s.to_string()), env, node_budget)
        }
        _ => return Err(not_implemented("unknown", target)),
    };
    // 解析済みの定数ノードを通常の束縛へ流す（TIMESTAMPTZ のセッション TZ 確定もここで行う）。
    bind_expr_any(&literal, env, node_budget)
}

fn charge_node(node_budget: &mut usize) -> Result<(), SqlSurfaceError> {
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    Ok(())
}

/// `CAST(<operand> AS <target>)` の束縛（`udf_call::bind_expr_any` の `Expr::Cast` 腕から呼ばれる）。
///
/// 束縛の結果は次のいずれか。ここで値の変換は行わない（兄弟 Issue の担当）。
/// - `42846`: 規則表で `×` の組（型の組だけで決まり、値が NULL でも同じ）。
/// - 恒等: 同一型で型修飾子なし。内側の束縛結果をそのまま返す。
/// - 型なし文字列リテラルを入力として解釈、または型付き NULL。
/// - それ以外は `0A000`。
pub(super) fn bind_cast(
    operand: &Expr,
    target: &CastType,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let target_ty = target.expr_type();
    match operand {
        // 型付き NULL（`CAST(NULL AS integer)`）。変換先に式の型が無ければ未実装。
        Expr::Null => {
            charge_node(node_budget)?;
            match target_ty {
                Some(ty) => Ok((BoundExpr::Null, ty)),
                None => Err(not_implemented("unknown", target)),
            }
        }
        Expr::String(s) => bind_string_literal_as(s, target, env, node_budget),
        _ => {
            let (bound, ty) = bind_expr_any(operand, env, node_budget)?;
            let source = source_cast_kind(operand, &bound, ty, env.schema());
            let from_kind = match source {
                SourceKind::Known(k) => k,
                SourceKind::Outside => return Err(not_implemented("vector", target)),
                SourceKind::UnknownNumeric => {
                    // 数値の行どうしは変換先によって規則が異なる。数値 6 型と bool では
                    // 誤った `42846`／恒等を作らないよう未実装とする。他の変換先では全数値行が同じ規則。
                    if matches!(
                        target.kind(),
                        CastKind::Int4
                            | CastKind::Int8
                            | CastKind::Int2
                            | CastKind::Float4
                            | CastKind::Float8
                            | CastKind::Numeric
                            | CastKind::Bool
                    ) {
                        return Err(not_implemented("numeric", target));
                    }
                    CastKind::Int4
                }
            };
            // 配列は式の型を持たないため、変換元に配列が現れることはない。
            let from_ty_rule = cast_rule(from_kind, target.kind());
            match from_ty_rule {
                CastRule::None => Err(SqlSurfaceError::CannotCoerce {
                    from: from_kind.label().to_string(),
                    to: target.pg_label(),
                }),
                CastRule::Identity if !target.has_modifier() && target_ty == Some(ty) => {
                    Ok((bound, ty))
                }
                _ => Err(not_implemented(from_kind.label(), target)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ddl_column_type::parse_column_type_name;
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
        let name = parse_column_type_name(&tokens, &mut pos)?;
        assert_eq!(pos, tokens.len(), "all tokens consumed for {sql}");
        CastType::from_sql_type_name(&name)
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
            CastType::Numeric {
                precision: 10,
                scale: 2,
            },
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
        assert_eq!(CastType::Uuid.expr_type(), None);
        assert_eq!(CastType::Array(Box::new(CastType::Int4)).expr_type(), None);
    }
}
