//! `core/plpgsql_validate.rs` の単体テスト（Issue #2224・SQL-68。ADR D2・D3）。
//!
//! 本物の `EngineCore`（既存の許可リスト解析）へ委譲した定義時のダミー検証を固定する。
//! 構文誤り・範囲外は定義時に拒否し、未知の表などは呼び出し時の検証へ回す（late binding）。

use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::plpgsql::ast::{ParamDecl, ParseOptions, ReturnShape};
use crate::plpgsql::embedded::validate_embedded;
use crate::plpgsql::parse_body;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::storage::Storage;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn new_core() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("plpgsql-validate");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn check(core: &EngineCore, src: &str) -> Result<(), SqlSurfaceError> {
    let opts = ParseOptions::new(
        vec![ParamDecl {
            name: Some("p".to_string()),
            ty: SqlColumnTypeName::Integer,
        }],
        ReturnShape::Scalar,
    );
    let body = parse_body(src, &opts)?;
    validate_embedded(&body, core)
}

#[test]
fn unknown_table_passes_at_definition_time() {
    let (core, _g) = new_core();
    check(
        &core,
        "DECLARE n int; BEGIN SELECT count(*) INTO n FROM no_such_table; RETURN n; END",
    )
    .expect("late binding");
}

#[test]
fn syntax_error_is_rejected_with_42601() {
    let (core, _g) = new_core();
    let e = check(&core, "BEGIN SELECT FROM WHERE; END").expect_err("syntax");
    assert_eq!(e.wire_code(), "42601");
}

#[test]
fn valid_insert_and_variable_expressions_pass() {
    let (core, _g) = new_core();
    check(
        &core,
        "DECLARE v int := 1; BEGIN \
         INSERT INTO docs (id, embedding, lang) VALUES (v, '[0.1,0.2]', 'en'); \
         PERFORM v + 1; \
         IF v + p > 3 THEN v := v + 1; END IF; \
         RETURN v; END",
    )
    .expect("valid body");
}

#[test]
fn variable_in_identifier_position_is_not_replaced() {
    let (core, _g) = new_core();
    check(
        &core,
        "DECLARE lang text := 'en'; BEGIN \
         INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2]', lang); END",
    )
    .expect("column list keeps the identifier");
}

#[test]
fn unsupported_embedded_syntax_is_rejected_at_definition_time() {
    let (core, _g) = new_core();
    // 許可リスト外の構文（式の位置の未対応演算子など）は 42601／0A000 のどちらかで拒否される。
    let e = check(&core, "BEGIN SELECT 1 FROM docs WHERE lang @@@ 'x'; END").expect_err("reject");
    assert!(matches!(e.wire_code(), "42601" | "0A000"));
}
#[test]
fn delegated_syntax_error_does_not_echo_fragments() {
    let (core, _g) = new_core();
    let e = check(
        &core,
        "BEGIN PERFORM secret_col_xyz FROM WHERE secret_w_xyz; END",
    )
    .expect_err("syntax");
    assert_eq!(e.wire_code(), "42601");
    assert!(!e.to_string().contains("xyz"), "leaked: {e}");
}
