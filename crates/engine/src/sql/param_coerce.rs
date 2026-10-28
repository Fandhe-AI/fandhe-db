//! Parse で宣言された型と文脈の型の変換可否の判定（Issue #2437・WIRE-24 (1)(5)・
//! SQL-41・SQL-45 のポインタ）。
//!
//! 責務境界: `core.rs::EngineCore::parse_sql_prepared_with_declared` が構造検証 parse の
//! 成功後に呼び、宣言型（[`DeclaredParamType`]）が `$n` の文脈の型（代入先の列・比較の相手・
//! 組み込み関数の引数）へ変換できるかだけを判定する。規則は `sql::cast` の規則表
//! （`docs/design/cast-conversion-rules.md`）を引き、第 2 の規則表は持たない。
//! 値には触れず、カタログの列型・宣言型・構文の形だけで決まるため、他テナントの行や
//! 表の存在を観測させない。文脈が引けない位置は判定対象に含めず、宣言なしの場合と同じ
//! 後段のエラーに委ねる（宣言の有無で応答コードが変わる存在オラクルを作らない）。
//!
//! 判定規則（文脈の型は ParameterDescription で公告する型と一致させる）:
//!
//! - 代入（`SET c = $n`・`INSERT VALUES`・括弧付き `$n` 単独）: 規則が恒等・暗黙・代入・
//!   入出力代入のとき受理、それ以外は `42804`。
//! - 比較・演算子: 宣言型から文脈の型、または逆向きのどちらかが恒等・暗黙のとき受理、
//!   それ以外は `42883`（PG の型をまたぐ演算子で、暗黙の変換でどちらかに寄せられる組を通す）。
//! - 組み込み関数の文字列引数: 宣言型から `text` が恒等・暗黙のとき受理、それ以外は `42883`。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast::{cast_rule, kind_of_column, CastKind, CastRule};
use crate::sql::exec::ColumnMeta;
use crate::sql::params::{DeclaredParamType, PreparedParamType};

/// `$n` 出現の位置の種別（判定規則の切り替えに使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamContextKind {
    /// 代入先の列への書き込み位置。
    Assignment,
    /// 比較・算術演算子の被演算子位置。
    Operator,
    /// 組み込み関数の文字列引数位置。
    FunctionArg,
    /// `ON CONFLICT DO UPDATE SET <列> = <列> + $n` の加算オペランド位置（Issue #2427・SQL-49）。
    /// 規則は [`Self::Operator`] と同じだが、整数の代入先へ非整数の数値型（float4／float8／
    /// numeric）を宣言した場合はリテラル経路（整数列への非整数リテラルの加算）と揃えて
    /// `0A000` にする。
    AddOperand,
}

/// `$n` の 1 出現についての判定入力。`infer_param_types` が構文とカタログから導く。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParamCheck {
    /// パラメータ番号（1 始まり）。
    pub(crate) param: u16,
    pub(crate) kind: ParamContextKind,
    /// 文脈の型（ParameterDescription で公告する型と一致させた種別）。
    pub(crate) ctx: CastKind,
}

impl DeclaredParamType {
    /// 規則表の見出し（[`CastKind`]）への写像。網羅 `match`（型の追加時に判定漏れを
    /// コンパイルエラーにする）。
    #[deny(clippy::wildcard_enum_match_arm)]
    pub(crate) fn cast_kind(self) -> CastKind {
        match self {
            DeclaredParamType::Int2 => CastKind::Int2,
            DeclaredParamType::Int4 => CastKind::Int4,
            DeclaredParamType::Int8 => CastKind::Int8,
            DeclaredParamType::Float4 => CastKind::Float4,
            DeclaredParamType::Float8 => CastKind::Float8,
            DeclaredParamType::Bool => CastKind::Bool,
            DeclaredParamType::Text => CastKind::Text,
            DeclaredParamType::Varchar => CastKind::Varchar,
            DeclaredParamType::Bpchar => CastKind::Bpchar,
            DeclaredParamType::Numeric => CastKind::Numeric,
            DeclaredParamType::Date => CastKind::Date,
            DeclaredParamType::Timestamp => CastKind::Timestamp,
            DeclaredParamType::TimestampTz => CastKind::TimestampTz,
            DeclaredParamType::Interval => CastKind::Interval,
            DeclaredParamType::Time => CastKind::Time,
            DeclaredParamType::Bytea => CastKind::Bytea,
            DeclaredParamType::Uuid => CastKind::Uuid,
            DeclaredParamType::Json => CastKind::Json,
            DeclaredParamType::Jsonb => CastKind::Jsonb,
        }
    }
}

/// 推論スロット型から文脈の型の種別を導く。ParameterDescription の公告（`id` は numeric、
/// ベクトル・enum・配列列と型なしは text）に合わせる。公告した OID をそのまま宣言した
/// クライアントが拒否されないことを不変条件とする。宣言型をスロット型にした位置
/// （[`PreparedParamType::Declared`]）は推論型が無いため判定対象外（`None`）。
pub(crate) fn context_kind(ty: &PreparedParamType) -> Option<CastKind> {
    match ty {
        PreparedParamType::Text | PreparedParamType::VectorText => Some(CastKind::Text),
        PreparedParamType::Declared(_) => None,
        PreparedParamType::Column(meta) => Some(match meta {
            ColumnMeta::Id | ColumnMeta::AliasedId { .. } => CastKind::Numeric,
            ColumnMeta::Scalar { ty, .. } => announced_kind(ty),
            ColumnMeta::Computed { ty, .. } => ty.as_ref().map_or(CastKind::Text, announced_kind),
            // SEARCH 順序列（record／record[]）は文脈型にならない。型なしと同じ text 扱い（SQL-35）。
            ColumnMeta::Record { .. } => CastKind::Text,
        }),
    }
}

/// 列型の公告上の種別。配列列は text（25）で公告されるため `Array` 種別にせず `Text` に合わせる。
fn announced_kind(ty: &crate::catalog::ColumnType) -> CastKind {
    match kind_of_column(ty) {
        Some(CastKind::Array) | None => CastKind::Text,
        Some(k) => k,
    }
}

fn is_implicit(rule: CastRule) -> bool {
    matches!(rule, CastRule::Identity | CastRule::Implicit)
}

/// 宣言型 `decl` が `check` の文脈へ変換できるかを判定する。文言は `$n` の番号と
/// 静的な型名だけで組み立てる（値・表名・OID の数値を含めない）。
pub(crate) fn check_declared(
    decl: DeclaredParamType,
    check: &ParamCheck,
) -> Result<(), SqlSurfaceError> {
    let from = decl.cast_kind();
    let to = check.ctx;
    let n = check.param;
    match check.kind {
        ParamContextKind::Assignment => {
            if matches!(
                cast_rule(from, to),
                CastRule::Identity
                    | CastRule::Implicit
                    | CastRule::Assignment
                    | CastRule::IoAssignment
            ) {
                Ok(())
            } else {
                Err(SqlSurfaceError::datatype_mismatch(format!(
                    "parameter ${n} of type {} cannot be assigned to a column of type {}",
                    from.label(),
                    to.label()
                )))
            }
        }
        ParamContextKind::Operator | ParamContextKind::AddOperand => {
            if check.kind == ParamContextKind::AddOperand
                && matches!(to, CastKind::Int2 | CastKind::Int4 | CastKind::Int8)
                && matches!(
                    from,
                    CastKind::Float4 | CastKind::Float8 | CastKind::Numeric
                )
            {
                return Err(SqlSurfaceError::feature_not_supported(format!(
                    "parameter ${n} of type {} added to an integer column is not supported",
                    from.label()
                )));
            }
            if is_implicit(cast_rule(from, to)) || is_implicit(cast_rule(to, from)) {
                Ok(())
            } else {
                Err(SqlSurfaceError::undefined_function(format!(
                    "operator does not exist: {} and {} (parameter ${n})",
                    to.label(),
                    from.label()
                )))
            }
        }
        ParamContextKind::FunctionArg => {
            if is_implicit(cast_rule(from, CastKind::Text)) {
                Ok(())
            } else {
                Err(SqlSurfaceError::undefined_function(format!(
                    "function argument of type text required; parameter ${n} is of type {}",
                    from.label()
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnType;

    fn chk(kind: ParamContextKind, ctx: CastKind) -> ParamCheck {
        ParamCheck {
            param: 1,
            kind,
            ctx,
        }
    }

    #[test]
    fn assignment_follows_rule_table() {
        use ParamContextKind::Assignment as A;
        // 受理: 代入（a）・暗黙（i）・入出力代入（io/a）。
        assert!(check_declared(DeclaredParamType::Int8, &chk(A, CastKind::Int4)).is_ok());
        assert!(check_declared(DeclaredParamType::Float8, &chk(A, CastKind::Int4)).is_ok());
        assert!(check_declared(DeclaredParamType::Int4, &chk(A, CastKind::Float8)).is_ok());
        assert!(check_declared(DeclaredParamType::Uuid, &chk(A, CastKind::Text)).is_ok());
        assert!(check_declared(DeclaredParamType::Date, &chk(A, CastKind::TimestampTz)).is_ok());
        // 拒否: 明示（e）・入出力明示（io/e）・対応なし。
        let e = check_declared(DeclaredParamType::Int4, &chk(A, CastKind::Bool)).unwrap_err();
        assert_eq!(e.wire_code(), "42804");
        assert!(check_declared(DeclaredParamType::Text, &chk(A, CastKind::Int4)).is_err());
        assert!(check_declared(DeclaredParamType::Text, &chk(A, CastKind::Uuid)).is_err());
        assert!(check_declared(DeclaredParamType::Date, &chk(A, CastKind::Int4)).is_err());
    }

    #[test]
    fn operator_is_symmetric_and_implicit_only() {
        use ParamContextKind::Operator as O;
        // 対称: int4 列と int8 宣言（int8 → int4 は代入、int4 → int8 は暗黙）。
        assert!(check_declared(DeclaredParamType::Int8, &chk(O, CastKind::Int4)).is_ok());
        assert!(check_declared(DeclaredParamType::Float4, &chk(O, CastKind::Int4)).is_ok());
        assert!(check_declared(DeclaredParamType::Varchar, &chk(O, CastKind::Text)).is_ok());
        let e = check_declared(DeclaredParamType::Text, &chk(O, CastKind::Int4)).unwrap_err();
        assert_eq!(e.wire_code(), "42883");
        assert!(check_declared(DeclaredParamType::Int8, &chk(O, CastKind::Text)).is_err());
        assert!(check_declared(DeclaredParamType::Bool, &chk(O, CastKind::Int4)).is_err());
    }

    #[test]
    fn add_operand_rejects_non_integer_declared_type_for_integer_target() {
        use ParamContextKind::AddOperand as P;
        // 整数の代入先: 整数宣言は受理、非整数の数値宣言は `0A000`、非数値は `42883`。
        assert!(check_declared(DeclaredParamType::Int8, &chk(P, CastKind::Int4)).is_ok());
        for d in [
            DeclaredParamType::Float4,
            DeclaredParamType::Float8,
            DeclaredParamType::Numeric,
        ] {
            let e = check_declared(d, &chk(P, CastKind::Int4)).unwrap_err();
            assert_eq!(e.wire_code(), "0A000");
        }
        let e = check_declared(DeclaredParamType::Text, &chk(P, CastKind::Int4)).unwrap_err();
        assert_eq!(e.wire_code(), "42883");
        // 浮動小数の代入先: 数値宣言は Operator と同じ規則で受理。
        assert!(check_declared(DeclaredParamType::Int4, &chk(P, CastKind::Float8)).is_ok());
        assert!(check_declared(DeclaredParamType::Float8, &chk(P, CastKind::Float4)).is_ok());
    }

    #[test]
    fn function_arg_requires_text_convertible() {
        use ParamContextKind::FunctionArg as F;
        assert!(check_declared(DeclaredParamType::Varchar, &chk(F, CastKind::Text)).is_ok());
        let e = check_declared(DeclaredParamType::Int4, &chk(F, CastKind::Text)).unwrap_err();
        assert_eq!(e.wire_code(), "42883");
    }

    #[test]
    fn messages_carry_only_numbers_and_type_names() {
        use ParamContextKind::Assignment as A;
        let e = check_declared(DeclaredParamType::Text, &chk(A, CastKind::Int4)).unwrap_err();
        let msg = format!("{e:?}");
        assert!(msg.contains("$1") && msg.contains("text") && msg.contains("integer"));
    }

    #[test]
    fn declared_cast_kind_and_context_follow_advertised_types() {
        assert_eq!(
            DeclaredParamType::TimestampTz.cast_kind(),
            CastKind::TimestampTz
        );
        assert_eq!(DeclaredParamType::Jsonb.cast_kind(), CastKind::Jsonb);
        assert_eq!(context_kind(&PreparedParamType::Text), Some(CastKind::Text));
        assert_eq!(
            context_kind(&PreparedParamType::VectorText),
            Some(CastKind::Text)
        );
        assert_eq!(
            context_kind(&PreparedParamType::Declared(DeclaredParamType::Int4)),
            None
        );
        assert_eq!(
            context_kind(&PreparedParamType::Column(ColumnMeta::Id)),
            Some(CastKind::Numeric)
        );
        let scalar = |ty| {
            PreparedParamType::Column(ColumnMeta::Scalar {
                name: "c".into(),
                ty,
            })
        };
        assert_eq!(
            context_kind(&scalar(ColumnType::Integer)),
            Some(CastKind::Int4)
        );
    }
}
