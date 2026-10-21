//! `ALTER TABLE ... ALTER COLUMN ... TYPE ... [USING <式>]` の変換式の束縛と型検査、
//! および行ごとの変換エラーの固定文言化
//! （TABLE-40 のポインタ、Issue #1635・#1636。式の許可範囲は SQL-45、代入変換の規則は SQL-47）。
//!
//! 責務境界: 構文段（`sql::allowlist` の `ALTER COLUMN` 解析）が作る [`Expr`] を、表層
//! （`sql::ddl::execute_alter_table_alter_column_type`）が早期検証のために、カタログ
//! （`catalog::Storage::alter_table_alter_column_type_collated`）が**書き込み txn の内側で**
//! 旧スキーマに対して束縛し直す（事前判定との TOCTOU を作らない）。束縛結果
//! [`BoundAlterConversion`] は `column_rewrite` が全行の書き換えに使う。
//! `USING` 省略は対象列自身の参照（`USING <col>`）と同じ式として扱う。
//!
//! 行ごとの評価・変換エラーは、呼び出し元セッションから見えない全テナントの行の値を含みうる
//! （`cast_value` の入力エコー等）ため、[`redact_row_conversion_error`] が SQLSTATE を保ったまま
//! 固定文言へ置き換えてから返す。エラー文言に値・テナント・行 id を載せない。
//!
//! 静的エラー（束縛・型検査）の文言は固定の英語で、列名と静的な型ラベルだけを含める。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast::{
    bind_scalar_assignment, cast_rule, cast_type_assignable, cast_type_of_column,
    is_assignment_rule, source_cast_kind, CastKind, CastType, SourceKind,
};
use crate::sql::udf_call::{
    bind_array_assignment_source, first_non_builtin_call_with, BoundExpr, Expr, UdfRegistry,
    MAX_EXPR_NODES,
};

/// 数値 6 型（種別を静的に決められない数値式の保守的な判定に使う）。
const NUMERIC_KINDS: [CastKind; 6] = [
    CastKind::Int2,
    CastKind::Int4,
    CastKind::Int8,
    CastKind::Float4,
    CastKind::Float8,
    CastKind::Numeric,
];

/// 行ごとの変換エラーを固定文言へ丸めるときの汎用文言（値を含まない）。
const CONVERSION_FAILED: &str = "value conversion failed during column type conversion";

fn not_supported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: detail.to_string(),
    }
}

/// 束縛済みの列型変換（Issue #1636）。`bound` は旧スキーマの論理列 index で列を参照するため、
/// 評価は束縛に使ったスキーマと同一の行デコード結果に対してだけ行う
/// （カタログが同一 write txn 内で束縛し、同じ `old_schema` で行を読む）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundAlterConversion {
    /// 代入変換ノードまで包んだ束縛済みの式。
    pub(crate) bound: BoundExpr,
    /// 変換後の列型。
    pub(crate) target_ty: ColumnType,
    /// 変換後の列が NULL を許すか（`NULL` 結果の `23502` 判定用）。
    pub(crate) nullable: bool,
    /// 対象列名（`23502` の文言用。スキーマ由来で値ではない）。
    pub(crate) column_name: String,
}

/// `USING <列名>`（対象列だけを参照する式）か。省略と同じ扱い（行の値をそのまま代入変換）に
/// するための判定。
pub(crate) fn is_identity_using(expr: &Expr, column_name: &str) -> bool {
    matches!(expr, Expr::Ident(name) if name == column_name)
}

/// 変換式を旧スキーマに対して束縛し、結果が `target` へ代入変換できることを検査して、
/// 行ごとの評価に使える [`BoundAlterConversion`] を返す。`expr` が `None`（`USING` 省略）は
/// 対象列自身の参照として束縛する。`column_index` は変換対象列の論理 index（旧スキーマ上）。
///
/// 判定順: 組み込み以外の関数（`42601`）→ VECTOR（`42804`）・ENUM（`0A000`）→
/// 式の束縛（未定義列 `42703` 等）→ 結果型の代入変換（`42804`／種別不明は `0A000`）→
/// 代入変換ノードの生成。行は一切読まない。
pub(crate) fn bind_alter_conversion(
    expr: Option<&Expr>,
    schema: &TableSchema,
    column_index: usize,
    target: &ColumnType,
) -> Result<BoundAlterConversion, SqlSurfaceError> {
    let column = schema
        .columns
        .get(column_index)
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "USING target column index is out of range".to_string(),
        })?;
    let implicit = Expr::Ident(column.name.clone());
    let expr = expr.unwrap_or(&implicit);
    // 書き込み経路で UDF・WASM を実行しない。DDL には時刻スコープが無いため現在時刻関数も拒否する。
    if first_non_builtin_call_with(expr, false, true).is_some() {
        return Err(SqlSurfaceError::unsupported(
            "only built-in functions are allowed in a USING expression",
        ));
    }
    // VECTOR は次元固定（TABLE-19）のため、変換元・変換先どちらでも型変更を許さない。
    if matches!(column.ty, ColumnType::Vector(_)) || matches!(target, ColumnType::Vector(_)) {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "column {:?} cannot be converted with a USING expression to or from a vector type",
            column.name
        )));
    }
    // ENUM が変換元・変換先になる代入変換は未対応（規則表の外）。
    if matches!(column.ty, ColumnType::Enum(_)) {
        return Err(not_supported(
            "USING with an enum source type is not supported",
        ));
    }
    let Some(target_cast) = cast_type_of_column(target) else {
        return Err(not_supported(
            "USING with an enum target type is not supported",
        ));
    };

    // 単独の `NULL` はどの型へも代入変換できる（束縛段は CASE 等の外の `NULL` を拒否するため先に受理）。
    if matches!(expr, Expr::Null) {
        return Ok(BoundAlterConversion {
            bound: BoundExpr::Null,
            target_ty: target.clone(),
            nullable: column.nullable,
            column_name: column.name.clone(),
        });
    }
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    let (bound, ty, _) = bind_array_assignment_source(expr, schema, &registry, &mut budget)?;

    let convertible = match source_cast_kind(expr, &bound, ty, Some(schema)) {
        SourceKind::Known(k) => is_assignment_rule(cast_rule(k, target_cast.kind())),
        SourceKind::Array(Some(elem)) => {
            cast_type_assignable(&CastType::Array(Box::new(elem)), &target_cast)
        }
        // 種別を静的に決められない数値は、数値 6 型すべてが代入変換できるときだけ受理する。
        SourceKind::UnknownNumeric => NUMERIC_KINDS
            .iter()
            .all(|k| is_assignment_rule(cast_rule(*k, target_cast.kind()))),
        SourceKind::Array(None) | SourceKind::Outside => {
            return Err(not_supported(
                "the result type of this USING expression cannot be determined",
            ))
        }
    };
    if !convertible {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "result of USING clause for column {:?} cannot be cast automatically to type {}",
            column.name,
            target_cast.pg_label()
        )));
    }
    let bound = bind_scalar_assignment(expr, bound, ty, schema, target)?;
    Ok(BoundAlterConversion {
        bound,
        target_ty: target.clone(),
        nullable: column.nullable,
        column_name: column.name.clone(),
    })
}

/// 型変換の対象列の `DEFAULT`／補完値 1 つを、旧型から新型へ**代入変換**（SQL-47。`USING` 式は
/// 使わない。PostgreSQL と同じ）して新しい型の正準な [`ColumnDefault`] に直す（TABLE-40 (3)・
/// Issue #1637）。`catalog` の `ALTER COLUMN TYPE` の一般変換経路が、スキーマ書き戻しの前に
/// 呼ぶ。行は読まない。
///
/// - `CurrentTime`（時刻値関数）・`Sequence`（`DEFAULT nextval`）は構文形のまま、新しい型と整合するときだけ保持する。
/// - それ以外は旧型の値として実体化 → 1 列だけのスキーマ上で代入変換を評価 → 正準形へ戻す。
///   正準形は新しい型で束縛し直せることを確かめる（整数・bool・文字列は値も一致を要求し、
///   浮動小数・NUMERIC・日時は束縛成功をもって可とする）。
///
/// 失敗（変換不能・評価エラー・再束縛不能）はすべて値を含まない固定文言の `42804`。
pub(crate) fn convert_column_default_for_alter(
    old_column: &crate::catalog::ColumnDef,
    default: &crate::catalog::ColumnDefault,
    new_ty: &ColumnType,
) -> Result<crate::catalog::ColumnDefault, SqlSurfaceError> {
    use crate::catalog::{ColumnDef, ColumnDefault};
    use crate::row_codec::Value;
    let fail = || {
        SqlSurfaceError::datatype_mismatch(format!(
            "default for column {:?} cannot be cast automatically to the new column type",
            old_column.name
        ))
    };
    if let ColumnDefault::CurrentTime(t) = default {
        return if t.compatible_with(new_ty) {
            Ok(default.clone())
        } else {
            Err(fail())
        };
    }
    // IDENTITY 列の型変更は暗黙 SEQUENCE の型も変える必要があるが未対応（fail-closed。
    // TABLE-24・Issue #1695）。
    if let ColumnDefault::Identity { .. } = default {
        return Err(SqlSurfaceError::feature_not_supported(
            "ALTER COLUMN TYPE on an identity column is not supported",
        ));
    }
    // `DEFAULT nextval`（TABLE-24・Issue #1690）は構文形のまま、新しい型が整数 3 型のときだけ保持する。
    if let ColumnDefault::Sequence(_) = default {
        return if matches!(
            new_ty,
            ColumnType::SmallInt | ColumnType::Integer | ColumnType::BigInt
        ) {
            Ok(default.clone())
        } else {
            Err(fail())
        };
    }
    // 1 列だけの作業用スキーマ（NULL 許可）。旧列の名前・型・照合で値を実体化する。
    let eval_one = |col_ty: &ColumnType,
                    value: Value,
                    target: &ColumnType|
     -> Result<Value, SqlSurfaceError> {
        let mini = TableSchema::new(
            "alter_default_probe",
            vec![ColumnDef::new(
                old_column.name.clone(),
                col_ty.clone(),
                true,
            )],
        );
        let conv = bind_alter_conversion(None, &mini, 0, target).map_err(|_| fail())?;
        let metadata =
            crate::row_codec::encode_scalar_columns(&mini, &[value]).map_err(|_| fail())?;
        let view = crate::row_codec::scan_scalar_columns(&mini, &metadata).map_err(|_| fail())?;
        let evaluated = crate::sql::udf_call::eval_with_scalars(&conv.bound, 0, &[], &view)
            .map_err(|_| fail())?;
        crate::sql::set_expr::expr_value_to_column_value(
            &conv.target_ty,
            true,
            &conv.column_name,
            evaluated,
        )
        .map_err(|_| fail())
    };
    let old_value =
        crate::sql::parser::bind_column_default(old_column, default).map_err(|_| fail())?;
    let new_value = eval_one(&old_column.ty, old_value, new_ty)?;
    let candidate = match &new_value {
        Value::Bool(b) => ColumnDefault::Bool(*b),
        Value::Text(s) => ColumnDefault::Text(s.clone()),
        Value::TimestampTz(micros) => ColumnDefault::Text(crate::datetime::format_timestamptz(
            *micros,
            &crate::datetime::SessionTimeZone::utc(),
        )),
        Value::Integer(_)
        | Value::BigInt(_)
        | Value::Real(_)
        | Value::Double(_)
        | Value::Numeric(_) => match eval_one(new_ty, new_value.clone(), &ColumnType::Text)? {
            Value::Text(s) => ColumnDefault::Number(s),
            _ => return Err(fail()),
        },
        Value::Null => return Err(fail()),
        _ => match eval_one(new_ty, new_value.clone(), &ColumnType::Text)? {
            Value::Text(s) => ColumnDefault::Text(s),
            _ => return Err(fail()),
        },
    };
    // 保存可能長の検査: カタログは DEFAULT／補完値を `MAX_COLUMN_DEFAULT_LEN` バイトまでしか
    // 保存できない（CHAR(n) の空白詰め等で変換後に超過しうる）。超過を保存段階の 42601 に
    // 漏らさず、ここで固定文言の 42804 にする。呼び出し側は DEFAULT なら拒否、補完値なら破棄する。
    if let ColumnDefault::Text(s) | ColumnDefault::Number(s) = &candidate {
        if s.len() > crate::catalog::MAX_COLUMN_DEFAULT_LEN {
            return Err(fail());
        }
    }
    // 再束縛検証: 正準形が新しい型の DEFAULT として束縛できること。完全一致は、整数・bool・文字列
    // など正準 text 表現が一意な型だけで要求する。REAL／DOUBLE PRECISION／NUMERIC の scale／
    // TIMESTAMPTZ は text 表現が元と異なっても代入変換自体は成功しているため、束縛成功をもって可とする
    // （変換不能のみ 42804 で fail-closed）。
    let probe = ColumnDef::new(old_column.name.clone(), new_ty.clone(), true);
    match crate::sql::parser::bind_column_default(&probe, &candidate) {
        Ok(v) => {
            let exact = matches!(
                new_value,
                Value::Bool(_) | Value::Text(_) | Value::Integer(_) | Value::BigInt(_)
            );
            if !exact || v == new_value {
                Ok(candidate)
            } else {
                Err(fail())
            }
        }
        Err(_) => Err(fail()),
    }
}

/// 全行の変換で発生した評価・変換エラーを、SQLSTATE を保ったまま固定文言へ置き換える
/// （`column_rewrite` が呼ぶ）。書き換えは全テナント・全可視性の行が対象のため、呼び出し元
/// セッションから見えない行の値が、`cast_value` の入力エコー等でエラー文言に混ざりうる。
/// 応答へ違反したテナント・行・値を一切含めない。
///
/// 全 variant をワイルドカード無しで列挙する。新しい variant が追加されたとき、
/// コンパイラが「値を含みうるか」の判断を強制する。
pub(crate) fn redact_row_conversion_error(e: SqlSurfaceError) -> SqlSurfaceError {
    use SqlSurfaceError as E;
    match e {
        E::NumericOutOfRange { .. } => E::NumericOutOfRange {
            detail: "value out of range during column type conversion".to_string(),
        },
        E::InvalidTextRepresentation { .. } => E::InvalidTextRepresentation {
            detail: "invalid input value during column type conversion".to_string(),
        },
        E::StringDataRightTruncation { .. } => E::StringDataRightTruncation {
            detail: "value too long during column type conversion".to_string(),
        },
        E::DatetimeFieldOverflow { .. } => E::DatetimeFieldOverflow {
            detail: "datetime field overflow during column type conversion".to_string(),
        },
        E::InvalidDatetimeFormat { .. } => E::InvalidDatetimeFormat {
            detail: "invalid datetime format during column type conversion".to_string(),
        },
        E::IntervalFieldOverflow { .. } => E::IntervalFieldOverflow {
            detail: "interval field overflow during column type conversion".to_string(),
        },
        E::DivisionByZero { .. } => E::DivisionByZero {
            detail: "division by zero during column type conversion".to_string(),
        },
        E::PayloadTooLarge { .. } => E::PayloadTooLarge {
            detail: "value too large during column type conversion".to_string(),
        },
        E::InvalidInput { .. } => E::InvalidInput {
            detail: CONVERSION_FAILED.to_string(),
        },
        E::FeatureNotSupported { .. } => E::FeatureNotSupported {
            detail: "unsupported value conversion during column type conversion".to_string(),
        },
        E::DatatypeMismatch { .. } => E::DatatypeMismatch {
            detail: "datatype mismatch during column type conversion".to_string(),
        },
        E::UnsupportedSyntax { .. } => E::UnsupportedSyntax {
            detail: CONVERSION_FAILED.to_string(),
        },
        E::Internal { .. } => E::Internal {
            detail: CONVERSION_FAILED.to_string(),
        },
        // 行の値を載せない variant（静的文字列・固定文言・コードのみ）。
        E::NegativeRowCount { .. }
        | E::CardinalityViolation
        | E::MissingOperationId
        | E::IdConflict
        | E::DuplicateOperationId
        | E::OperationIdContentMismatch
        | E::InvalidParameterValue { .. }
        | E::IndeterminateDatatype { .. }
        | E::InvalidObjectDefinition { .. }
        | E::GroupingError { .. }
        | E::WindowingError { .. }
        | E::CollationMismatch
        | E::IndeterminateCollation
        | E::NullValueNotAllowed { .. }
        | E::MoreThanOneSqlJsonItem { .. }
        | E::NoSqlJsonItem { .. }
        | E::SqlJsonScalarRequired { .. }
        | E::InvalidTransactionState
        | E::ActiveSqlTransaction
        | E::NoActiveSqlTransaction
        | E::InFailedSqlTransaction
        | E::LockNotAvailable
        | E::MaterializedViewNotPopulated
        | E::SequenceNotYetDefined
        | E::LastvalNotYetDefined
        | E::SequenceGeneratorLimitExceeded { .. }
        | E::DuplicateSchema { .. }
        | E::InvalidSchemaName { .. }
        | E::ReservedName { .. }
        | E::GeneratedAlways { .. }
        | E::MaterializedViewNotRefreshableConcurrently
        | E::ViewNotAutoUpdatable { .. }
        | E::PartitionedDmlInTransaction
        | E::InsufficientPrivilege
        | E::SchemaDependencyForbidden
        | E::ConflictTargetNotVisible
        | E::InvalidCursorName
        | E::UniqueViolation
        | E::ColumnReferenceWithoutFrom
        | E::ForeignKeyViolation
        | E::JsonPathEval(_)
        | E::JsonPathParse(_) => e,
        // スキーマ由来の名前だけを持つ variant（行の値ではない）。
        E::UndefinedTable { .. }
        | E::DuplicateTable { .. }
        | E::DuplicateObject { .. }
        | E::DuplicateType { .. }
        | E::InvalidTableDefinition { .. }
        | E::ColumnInPrimaryKey { .. }
        | E::InvalidViewReplacement { .. }
        | E::DuplicateColumn { .. }
        | E::InvalidColumnReference
        | E::DependentObjectsStillExist { .. }
        | E::WrongObjectType { .. }
        | E::NotNullViolation { .. }
        | E::UndefinedObject { .. }
        | E::UndefinedType { .. }
        | E::UndefinedCollation { .. }
        | E::UndefinedColumn { .. }
        | E::CheckViolation { .. }
        | E::WithCheckOptionViolation
        | E::AmbiguousColumn { .. } => e,
        // 式評価では到達しない想定で、文言が値を含みうる残りの variant は固定文言の
        // `Internal` へ丸める（到達しても fail-closed）。
        E::CannotCoerce { .. }
        | E::TransactionFeatureNotSupported { .. }
        | E::PartialCompletion { .. }
        | E::PartitionedDmlCancelled { .. }
        | E::PartitionedJobBusy { .. }
        | E::InvalidForeignKey { .. }
        | E::UndefinedFunction { .. }
        | E::DuplicateFunction { .. } => E::Internal {
            detail: CONVERSION_FAILED.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_using_matches_only_the_target_column_ident() {
        assert!(is_identity_using(&Expr::Ident("c".into()), "c"));
        assert!(!is_identity_using(&Expr::Ident("d".into()), "c"));
        assert!(!is_identity_using(&Expr::Number("1".into()), "c"));
    }

    #[test]
    fn redaction_removes_echoed_values() {
        let secret = "SECRET-VALUE-tenant-b";
        let cases = vec![
            SqlSurfaceError::numeric_out_of_range(format!("{secret} out of range")),
            SqlSurfaceError::InvalidTextRepresentation {
                detail: format!("invalid input syntax for type integer: \"{secret}\""),
            },
            SqlSurfaceError::string_data_right_truncation(secret),
            SqlSurfaceError::datetime_field_overflow(secret),
            SqlSurfaceError::InvalidDatetimeFormat {
                detail: secret.to_string(),
            },
            SqlSurfaceError::IntervalFieldOverflow {
                detail: secret.to_string(),
            },
            SqlSurfaceError::DivisionByZero {
                detail: secret.to_string(),
            },
            SqlSurfaceError::payload_too_large(secret),
            SqlSurfaceError::InvalidInput {
                detail: secret.to_string(),
            },
            SqlSurfaceError::FeatureNotSupported {
                detail: secret.to_string(),
            },
            SqlSurfaceError::datatype_mismatch(secret),
            SqlSurfaceError::Internal {
                detail: secret.to_string(),
            },
            SqlSurfaceError::CannotCoerce {
                from: secret.to_string(),
                to: secret.to_string(),
            },
        ];
        for e in cases {
            let before = e.wire_code();
            let is_coerce = matches!(e, SqlSurfaceError::CannotCoerce { .. });
            let out = redact_row_conversion_error(e);
            assert!(!out.to_string().contains(secret), "value leaked: {out}");
            // 到達しない想定の `CannotCoerce` は `Internal` へ丸める。それ以外は SQLSTATE を保つ。
            if !is_coerce {
                assert_eq!(out.wire_code(), before);
            }
        }
    }
}
