//! CTAS（SQL-52、Issue #1677）の `EngineCore` 側ヘルパ。
//!
//! 責務境界: 検証済みの読み取り文を Describe（行を評価しない）して出力列を得て、
//! `sql::ctas` の純粋関数で表の列定義へ導くところまで。表の作成と行投入（#1678）・台帳（#1679）は
//! 担わない。呼び出し元は #1678 の実行腕で、DDL 権限ゲートの後に呼ぶ前提。
//! Describe は RLS の行評価経路に触れず、見えない元表は通常の SELECT と同じ `42P01` になる。

use super::{EngineCore, ParsedSql};
use crate::catalog::ColumnDef;
use crate::sql::allowlist::{SqlSurfaceError, Statement};

impl EngineCore {
    /// `statement` の出力列と任意の列名リストから CTAS の列定義を導く。
    #[allow(dead_code)] // 呼び出し元は #1678 の実行腕（本 Issue では crate 内テストのみ）。
    pub(crate) fn ctas_output_column_defs(
        &self,
        session: &crate::sql::mode::SessionState,
        statement: &Statement,
        column_names: &[String],
    ) -> Result<Vec<ColumnDef>, SqlSurfaceError> {
        let metas = self
            .describe_parsed_in_session_impl(
                session,
                &ParsedSql::Statement(statement.clone()),
                false,
                &[],
                &crate::result_limits::ResultLimits::default(),
            )?
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        let outputs = crate::sql::ctas::output_columns_from_metas(metas);
        crate::sql::ctas::derive_ctas_column_defs(&outputs, column_names)
    }
}
/// 実ストレージ上の元表から CTAS の列定義を導く結合テスト（SQL-52 (1)・(5)）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnType, RowIdentity};
    use crate::kernel::CpuScalarProvider;
    use crate::policy::PolicyContext;
    use crate::schema_limits::SchemaLimits;
    use crate::sql::ctas::ctas_validated_create_table;
    use crate::sql::ddl::build_table_schema;
    use crate::sql::mode::SessionState;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn fixture(label: &str) -> (CleanupGuard, EngineCore) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let core = EngineCore::from_storage(
            Storage::open(&path).expect("open storage"),
            Box::new(CpuScalarProvider),
        );
        (guard, core)
    }

    fn ddl(core: &EngineCore, sql: &str) {
        let ctx = PolicyContext::with_visibilities("t1", [Visibility::Public, Visibility::Private])
            .expect("tenant");
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
    }

    fn defs(
        core: &EngineCore,
        sql: &str,
        names: &[&str],
    ) -> Result<Vec<ColumnDef>, SqlSurfaceError> {
        let stmt = crate::sql::allowlist::validate_sql(sql, &core.storage).expect("valid select");
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        core.ctas_output_column_defs(&SessionState::default(), &stmt, &names)
    }

    fn setup(core: &EngineCore) {
        ddl(core, "CREATE TYPE mood AS ENUM ('happy', 'sad')");
        ddl(
            core,
            "CREATE TABLE src (a NUMERIC(10,2), b VARCHAR(8), c CHAR(4), d VECTOR(3), \
             e INTEGER[4], f mood, g TEXT)",
        );
    }

    #[test]
    fn sql52_1_qualified_types_flow_into_table_schema() {
        let (_g, core) = fixture("ctas-types");
        setup(&core);
        let d = defs(&core, "SELECT a, b, c, d, e, f, g FROM src LIMIT 1", &[]).unwrap();
        let v = ctas_validated_create_table("dst".to_string(), false, false, d);
        let schema = build_table_schema(Some(&core.storage), &v, &SchemaLimits::default()).unwrap();
        assert_eq!(schema.row_identity(), RowIdentity::Allocated);
        assert!(schema.primary_key().is_none());
        let src = core.storage.get_table_schema("src").expect("src");
        for (got, want) in schema.columns.iter().zip(src.columns.iter()) {
            assert_eq!(got.name, want.name);
            assert_eq!(got.ty, want.ty, "type of {}", got.name);
        }
        let nullable: Vec<bool> = schema.columns.iter().map(|c| c.nullable).collect();
        assert_eq!(nullable, [true, true, true, false, true, true, true]);
    }

    #[test]
    fn sql52_1_literals_and_expressions_have_types() {
        let (_g, core) = fixture("ctas-literals");
        setup(&core);
        // 文字列リテラルの投影は現行の SELECT 構文が受理しない（検証段で 42601）。
        // 受理される式は静的な型を持ち、Text 以外へ確定する。
        let d = defs(&core, "SELECT 1 + 1 AS n FROM src LIMIT 1", &[]).unwrap();
        assert!(d[0].ty != ColumnType::Text);
        let e = defs(&core, "SELECT 1 + 1 FROM src LIMIT 1", &[]).unwrap_err();
        assert_eq!(e.wire_code(), "42601");
        assert!(defs(&core, "SELECT 1 + 1 FROM src LIMIT 1", &["n"]).is_ok());
    }

    #[test]
    fn sql52_1_name_list_applies_to_described_columns() {
        let (_g, core) = fixture("ctas-id");
        setup(&core);
        let d = defs(&core, "SELECT g FROM src LIMIT 1", &["k"]).unwrap();
        assert_eq!(d[0].name, "k");
    }

    #[test]
    fn sql52_1_missing_source_fails_and_column_limit_applies() {
        let (_g, core) = fixture("ctas-limits");
        setup(&core);
        let missing =
            crate::sql::allowlist::validate_sql("SELECT a FROM nope LIMIT 1", &core.storage);
        assert_eq!(missing.unwrap_err().wire_code(), "42P01");
        let d = defs(&core, "SELECT a, b, c FROM src LIMIT 1", &[]).unwrap();
        let v = ctas_validated_create_table("dst".to_string(), false, false, d);
        let tight = SchemaLimits::new(2, 1024, 1024);
        let err = build_table_schema(Some(&core.storage), &v, &tight).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn sql52_1_enum_is_rejected_for_temp_route() {
        let (_g, core) = fixture("ctas-enum-temp");
        setup(&core);
        let d = defs(&core, "SELECT f FROM src LIMIT 1", &[]).unwrap();
        let v = ctas_validated_create_table("dst".to_string(), false, false, d);
        let err = build_table_schema(None, &v, &SchemaLimits::default()).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }
}
