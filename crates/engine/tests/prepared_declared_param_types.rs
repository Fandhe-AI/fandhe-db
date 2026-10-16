//! Parse で宣言された型（`DeclaredParamType`）を `$n` のスロット型と値の検証に用いる
//! 契約（Issue #2436）の結合テスト。
//!
//! ポインタ: WIRE-24・WIRE-12・SQL-45 (6)。wire-server が型 OID から写した宣言型を
//! `parse_sql_prepared_with_declared` へ渡す経路の engine 側を、実 `Storage` 越しに固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::params::{DeclaredParamType as D, PreparedParamType};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::with_visibilities("sys", [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE TABLE docs (n INTEGER, d DOUBLE PRECISION, flag BOOLEAN, t TEXT)",
    )
    .expect("create table");
    (core, CleanupGuard(path))
}

fn some(s: &str) -> Option<Vec<u8>> {
    Some(s.as_bytes().to_vec())
}

fn bind_code(
    core: &EngineCore,
    sql: &str,
    declared: &[Option<D>],
    values: &[&str],
) -> Result<(), String> {
    let prepared = core
        .parse_sql_prepared_with_declared(sql, declared)
        .map_err(|e| e.wire_code().to_string())?;
    let binds: Vec<Option<Vec<u8>>> = values.iter().map(|v| some(v)).collect();
    core.bind_prepared(&prepared, &binds)
        .map(|_| ())
        .map_err(|e: SqlSurfaceError| e.wire_code().to_string())
}

#[test]
fn indeterminate_expression_uses_declared_type() {
    let (core, _g) = setup("decl-expr");
    let sql = "UPDATE docs SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'op-1'";
    let prepared = core
        .parse_sql_prepared_with_declared(sql, &[Some(D::Int4), Some(D::Int4)])
        .expect("declared types resolve the expression");
    assert_eq!(
        prepared.param_types(),
        &[
            PreparedParamType::Declared(D::Int4),
            PreparedParamType::Declared(D::Int4)
        ]
    );
    let bound = core
        .bind_prepared(&prepared, &[some("2"), some("3")])
        .expect("bind");
    let literal = core
        .parse_sql("UPDATE docs SET n = 2 + 3 WHERE id = 1 USING OPERATION_ID 'op-1'")
        .expect("literal form");
    assert_eq!(bound, literal);
}

#[test]
fn partially_declared_expression_keeps_42p18() {
    let (core, _g) = setup("decl-partial");
    let sql = "UPDATE docs SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'op-1'";
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int4), None], &["1", "2"]),
        Err("42P18".to_string())
    );
    assert_eq!(
        bind_code(&core, sql, &[], &["1", "2"]),
        Err("42P18".to_string())
    );
}

#[test]
fn unsupported_declared_type_inside_expression_is_0a000() {
    let (core, _g) = setup("decl-expr-unsupported");
    let sql = "UPDATE docs SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'op-1'";
    for ty in [D::Bool, D::Date, D::Timestamp, D::Numeric, D::Uuid] {
        assert_eq!(
            bind_code(&core, sql, &[Some(ty), Some(ty)], &["1", "2"]),
            Err("0A000".to_string()),
            "{ty:?}"
        );
    }
}

#[test]
fn unreferenced_number_takes_declared_type() {
    let (core, _g) = setup("decl-unref");
    let prepared = core
        .parse_sql_prepared_with_declared(
            "SELECT id FROM docs WHERE n = $2 ORDER BY id LIMIT 5",
            &[Some(D::Date), None],
        )
        .expect("parse");
    assert_eq!(
        prepared.param_types().first(),
        Some(&PreparedParamType::Declared(D::Date))
    );
}

#[test]
fn integer_declared_type_validates_range_and_syntax() {
    let (core, _g) = setup("decl-int");
    let sql = "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5";
    let d = [Some(D::Int2)];
    assert_eq!(bind_code(&core, sql, &d, &["40000"]), Err("22003".into()));
    assert_eq!(bind_code(&core, sql, &d, &["-32769"]), Err("22003".into()));
    assert_eq!(bind_code(&core, sql, &d, &["x"]), Err("22P02".into()));
    assert_eq!(bind_code(&core, sql, &d, &["7"]), Ok(()));
    assert_eq!(bind_code(&core, sql, &d, &["-32768"]), Ok(()));
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int4)], &["2147483648"]),
        Err("22003".into())
    );
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int8)], &["9223372036854775808"]),
        Err("22003".into())
    );
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int8)], &["9223372036854775807"]),
        Ok(())
    );
    // 巨大な桁数でも panic せず範囲外になる。
    let huge = "9".repeat(200);
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int8)], &[huge.as_str()]),
        Err("22003".into())
    );
}

#[test]
fn float_declared_on_double_column_rejects_fraction_for_int4() {
    let (core, _g) = setup("decl-int-on-double");
    let sql = "SELECT id FROM docs WHERE d = $1 ORDER BY id LIMIT 5";
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int4)], &["1.5"]),
        Err("22P02".into())
    );
    assert_eq!(bind_code(&core, sql, &[Some(D::Int4)], &["2"]), Ok(()));
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Float4)], &["1e40"]),
        Err("22003".into())
    );
    assert_eq!(bind_code(&core, sql, &[Some(D::Float8)], &["1.5"]), Ok(()));
}

#[test]
fn bool_declared_type_uses_pg_bool_grammar() {
    let (core, _g) = setup("decl-bool");
    let sql = "SELECT id FROM docs WHERE flag = $1 ORDER BY id LIMIT 5";
    assert_eq!(bind_code(&core, sql, &[Some(D::Bool)], &["yes"]), Ok(()));
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Bool)], &["maybe"]),
        Err("22P02".into())
    );
}

#[test]
fn error_message_does_not_contain_value() {
    let (core, _g) = setup("decl-msg");
    let prepared = core
        .parse_sql_prepared_with_declared(
            "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5",
            &[Some(D::Int2)],
        )
        .expect("parse");
    let err = core
        .bind_prepared(&prepared, &[some("987654")])
        .expect_err("out of range");
    let text = format!("{err}");
    assert!(!text.contains("987654"), "{text}");
    assert!(text.contains("$1"), "{text}");
}

#[test]
fn no_declaration_is_identical_to_legacy_parse() {
    let (core, _g) = setup("decl-regression");
    let sql = "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5";
    let legacy = core.parse_sql_prepared(sql).expect("legacy");
    let none = core
        .parse_sql_prepared_with_declared(sql, &[None])
        .expect("none");
    assert_eq!(legacy.param_types(), none.param_types());
    assert!(legacy.declared_param_types().iter().all(Option::is_none));
    let a = core.bind_prepared(&legacy, &[some("5")]).expect("a");
    let b = core.bind_prepared(&none, &[some("5")]).expect("b");
    assert_eq!(a, b);
}

#[test]
fn surplus_declarations_are_ignored_by_engine() {
    let (core, _g) = setup("decl-surplus");
    let prepared = core
        .parse_sql_prepared_with_declared(
            "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5",
            &[Some(D::Int4), Some(D::Int4), Some(D::Int4)],
        )
        .expect("surplus is wire's concern");
    assert_eq!(prepared.declared_param_types().len(), 1);
}

#[test]
fn inferred_slot_type_is_not_overridden_by_declaration() {
    let (core, _g) = setup("decl-keep-inferred");
    let prepared = core
        .parse_sql_prepared_with_declared(
            "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5",
            &[Some(D::Varchar)],
        )
        .expect("parse");
    assert!(matches!(
        prepared.param_types().first(),
        Some(PreparedParamType::Column(_))
    ));
    assert_eq!(prepared.declared_param_types(), &[Some(D::Varchar)]);
}
