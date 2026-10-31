//! `CREATE FUNCTION ... LANGUAGE plpgsql` の定義時検証（Issue #2217・SQL-67 (3)・SQL-68）。
//!
//! 役割: `core.rs` の `Statement::CreateFunction` 腕が、カタログへ書く前にここを呼ぶ。本体を
//! `plpgsql::parse_body`（構文誤り `42601`・範囲外 `0A000`・本体長 `54000`）で解析し、続けて
//! `validate_embedded`（埋め込み SQL の許可リスト解析。`EngineCore` が `EmbeddedSqlChecker`）で
//! 定義時に拒否する。AST は保存しない（原文を保存し、呼び出し時に再解析する。設計記録
//! `docs/design/plpgsql-subset.md` の D2）。
//!
//! 戻り値の形は `RETURNS trigger` なら `ReturnShape::Trigger`、それ以外のスカラーは
//! `ReturnShape::Scalar`（集合返却・`void` は構文段が `0A000`）。エラー文言は固定の英語で、本体の
//! 断片・関数名・テナント情報を含めない。

use super::EngineCore;
use crate::plpgsql::ast::{ParamDecl, ParseOptions, ReturnShape};
use crate::plpgsql::embedded::validate_embedded;
use crate::plpgsql::parse_body;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast::CastType;
use crate::sql::ddl_column_type::SqlColumnTypeName;

/// 関数シグネチャの型（[`CastType`]）を PL/pgSQL の引数宣言の型名へ写す。`numeric` の修飾子なしは
/// 定義時検証用のダミー値を作れる固定の精度で表す（呼び出し時の型は別途カタログの保存値から決まる）。
fn sql_type_name_of(ty: &CastType) -> Result<SqlColumnTypeName, SqlSurfaceError> {
    Ok(match ty {
        CastType::Int2 => SqlColumnTypeName::SmallInt,
        CastType::Int4 => SqlColumnTypeName::Integer,
        CastType::Int8 => SqlColumnTypeName::BigInt,
        CastType::Float4 => SqlColumnTypeName::Real,
        CastType::Float8 => SqlColumnTypeName::Double,
        CastType::Numeric(Some((p, s))) => SqlColumnTypeName::Numeric {
            precision: *p,
            scale: *s,
        },
        CastType::Numeric(None) => SqlColumnTypeName::Numeric {
            precision: 38,
            scale: 16,
        },
        CastType::Bool => SqlColumnTypeName::Boolean,
        CastType::Text => SqlColumnTypeName::Text,
        CastType::Varchar(n) => SqlColumnTypeName::Varchar(*n),
        CastType::Bpchar(n) => SqlColumnTypeName::Bpchar(*n),
        CastType::Date => SqlColumnTypeName::Date,
        CastType::Time => SqlColumnTypeName::Time,
        CastType::Timestamp => SqlColumnTypeName::Timestamp,
        CastType::TimestampTz => SqlColumnTypeName::TimestampTz,
        CastType::Interval => SqlColumnTypeName::Interval,
        CastType::Json => SqlColumnTypeName::Json,
        CastType::Jsonb => SqlColumnTypeName::Jsonb,
        CastType::Uuid => SqlColumnTypeName::Uuid,
        CastType::Bytea => SqlColumnTypeName::Bytea,
        CastType::Array(_) => {
            let mut ndims: u8 = 0;
            let mut cur = ty;
            while let CastType::Array(inner) = cur {
                ndims = ndims.saturating_add(1);
                cur = inner;
            }
            SqlColumnTypeName::Array {
                elem: Box::new(sql_type_name_of(cur)?),
                max_len: None,
                ndims,
            }
        }
    })
}

impl EngineCore {
    /// `LANGUAGE plpgsql` の本体を定義時に検証する（保存はしない）。`params` は正規化前の名前、
    /// `param_types` は全要素 `Some`（構文段が型なし引数を拒否済み）。呼び出し元は `core.rs` の
    /// `Statement::CreateFunction` 腕で、`require_ddl_permission` の後・カタログ書き込みの前。
    pub(crate) fn validate_plpgsql_definition(
        &self,
        params: &[String],
        param_types: &[Option<CastType>],
        returns_trigger: bool,
        body_source: &str,
    ) -> Result<(), SqlSurfaceError> {
        let decls = params
            .iter()
            .zip(param_types.iter())
            .map(|(name, ty)| {
                let ty = ty.as_ref().ok_or_else(|| {
                    SqlSurfaceError::unsupported("function parameters must be typed")
                })?;
                Ok(ParamDecl {
                    name: Some(name.to_ascii_lowercase()),
                    ty: sql_type_name_of(ty)?,
                })
            })
            .collect::<Result<Vec<_>, SqlSurfaceError>>()?;
        let shape = if returns_trigger {
            ReturnShape::Trigger
        } else {
            ReturnShape::Scalar
        };
        let limits = &self.plpgsql_limits;
        let opts = ParseOptions::new(decls, shape)
            .with_limits(limits.max_nesting_depth(), limits.max_body_bytes());
        let mut body = parse_body(body_source, &opts)?;
        validate_embedded(&mut body, self)
    }
}
#[cfg(test)]
mod tests {
    use crate::catalog::function::{FunctionLanguage, Parallel, Volatility};
    use crate::core::EngineCore;
    use crate::kernel::CpuScalarProvider;
    use crate::policy::PolicyContext;
    use crate::sql::mode::SessionState;
    use crate::storage::{Storage, Visibility};

    /// Issue #2217: 言語・揮発性・STRICT・PARALLEL・COST・`RETURNS trigger` がカタログへ保存され、
    /// 開き直した後も同じ値で読める（受け入れ条件 1）。
    #[test]
    fn attributes_are_persisted_and_survive_reopen() {
        let path = crate::test_util::temp_db::unique_db_path("fn-define-attrs");
        let _guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let ctx = PolicyContext::with_visibilities("t", [Visibility::Public]).expect("tenant");
        {
            let core = EngineCore::from_storage(
                Storage::open(&path).expect("open"),
                Box::new(CpuScalarProvider),
            );
            let mut s = SessionState::default();
            s.allow_ddl();
            for sql in [
                "CREATE FUNCTION a(x int) RETURNS int LANGUAGE sql IMMUTABLE STRICT \
                 PARALLEL SAFE COST 2.5 AS $$ SELECT x $$",
                "CREATE FUNCTION b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
            ] {
                core.execute_sql_in_session(&ctx, &mut s, sql).expect(sql);
            }
        }
        let storage = Storage::open(&path).expect("reopen");
        let defs = storage.list_functions("t").expect("list");
        let a = defs.iter().find(|d| d.function_key == "a").expect("a");
        assert_eq!(a.language, FunctionLanguage::Sql);
        assert_eq!(a.attrs.volatility, Volatility::Immutable);
        assert!(a.attrs.strict);
        assert_eq!(a.attrs.parallel, Parallel::Safe);
        assert_eq!(a.attrs.cost, Some(2.5));
        assert_eq!(a.body_source.trim(), "SELECT x");
        assert!(!a.attrs.allows_writes());
        let b = defs.iter().find(|d| d.function_key == "b").expect("b");
        assert_eq!(b.language, FunctionLanguage::Plpgsql);
        assert!(b.return_type.as_ref().is_some_and(|t| t.is_trigger()));
        assert!(b.attrs.allows_writes());
    }
}
