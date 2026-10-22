//! `INSERT ... ON CONFLICT ... DO UPDATE SET <col> = <col> + $n`（拡張クエリの Parse／Bind）の
//! `$n` の型推論と受理（SQL-49・WIRE-12、Issue #2427）の結合テスト。
//!
//! ポインタ: `docs/spec/04-behavior/` の SQL-49・WIRE-12。`EngineCore::parse_sql_prepared*`
//! → `PreparedSql::param_types` → `bind_prepared` → `execute_parsed_in_session` を production
//! 経路として検証する。`$n` の型は INSERT 対象表の代入先列から推論され（ParameterDescription の
//! 元になる）、型が合わない場合は fail-closed で拒否される。Bind 後の文は同じ値をリテラルで
//! 書いた SQL と同一になるため、加算・範囲検査・台帳は #2425 の既存経路がそのまま担う。
//! 並行 64 本の加算・不可視な衝突先の扱い（#2428）は対象外。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::params::{DeclaredParamType as D, PreparedParamType};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
// 疑似列 `id` を持つ従来方式の表で検証する（TABLE-23・Issue #2199）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "counters";

static OP_SEQ: AtomicU64 = AtomicU64::new(0);

fn op() -> String {
    format!("op-addp-{}", OP_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("big", ColumnType::BigInt, true),
            ColumnDef::new("sm", ColumnType::SmallInt, true),
            ColumnDef::new("r", ColumnType::Real, true),
            ColumnDef::new("d", ColumnType::Double, true),
            ColumnDef::new("t", ColumnType::Text, true),
            ColumnDef::new(
                "num",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
        ],
    )
}

fn open_engine(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    (core, path)
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(ctx, &mut session, sql)
}

fn seed(core: &EngineCore, ctx: &PolicyContext, id: u64, cols: &str, vals: &str) {
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, {cols}) VALUES ({id}, '[0.1,0.2]', {vals}) \
         USING OPERATION_ID '{}'",
        op()
    );
    run(core, ctx, &sql).expect(&sql);
}

fn cell(core: &EngineCore, ctx: &PolicyContext, id: u64, col: &str) -> Cell {
    let sql = format!("SELECT {col} FROM {TABLE} WHERE id = {id} LIMIT 10");
    let SqlOutcome::Query(result) = run(core, ctx, &sql).expect(&sql) else {
        panic!("expected Query outcome");
    };
    assert_eq!(result.rows.len(), 1, "{sql}");
    result.rows[0].cells[0].clone()
}

/// 加算形の upsert 文（`$1` が加算オペランド）。
fn add_sql(col: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, {col}) VALUES (1, '[0.1,0.2]', 0) \
         ON CONFLICT (id) DO UPDATE SET {col} = {col} + $1 USING OPERATION_ID '{}'",
        op()
    )
}

/// Parse → Bind → Execute。エラーは SQLSTATE を返す。
fn exec_add(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
    value: &str,
) -> Result<(), &'static str> {
    let prepared = core
        .parse_sql_prepared(sql)
        .unwrap_or_else(|e| panic!("{sql} must parse, got {e:?}"));
    let parsed = core
        .bind_prepared(&prepared, &[Some(value.as_bytes().to_vec())])
        .map_err(|e| e.wire_code())?;
    let mut session = SessionState::default();
    core.execute_parsed_in_session(ctx, &mut session, &parsed)
        .map(|_| ())
        .map_err(|e| e.wire_code())
}

fn parse_code(core: &EngineCore, sql: &str, declared: Option<D>) -> Option<&'static str> {
    core.parse_sql_prepared_with_declared(sql, &[declared])
        .err()
        .map(|e| e.wire_code())
}

fn slot_type(core: &EngineCore, sql: &str) -> PreparedParamType {
    core.parse_sql_prepared(sql)
        .expect("parse")
        .param_types()
        .first()
        .cloned()
        .expect("slot")
}

fn int(v: i64) -> Cell {
    Cell::SignedInteger(v)
}

#[test]
fn param_type_follows_the_assigned_column() {
    let (core, path) = open_engine("sql49-addp-types");
    let _g = CleanupGuard(path);
    for (col, expected) in [
        ("n", ColumnType::Integer),
        ("big", ColumnType::BigInt),
        ("sm", ColumnType::SmallInt),
        ("r", ColumnType::Real),
        ("d", ColumnType::Double),
    ] {
        match slot_type(&core, &add_sql(col)) {
            PreparedParamType::Column(ColumnMeta::Scalar { ty, .. }) => {
                assert_eq!(ty, expected, "{col}")
            }
            other => panic!("{col}: unexpected slot type {other:?}"),
        }
    }
}

#[test]
fn bound_values_add_like_the_literal_form() {
    let (core, path) = open_engine("sql49-addp-exec");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "n, big, r, d", "10, 5, 1.5, 2.25");
    exec_add(&core, &ctx, &add_sql("n"), "5").expect("add");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(15));
    exec_add(&core, &ctx, &add_sql("n"), "-3").expect("add negative");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(12));
    exec_add(&core, &ctx, &add_sql("big"), "9007199254740993").expect("bigint");
    assert_eq!(cell(&core, &ctx, 1, "big"), int(9_007_199_254_740_998));
    exec_add(&core, &ctx, &add_sql("d"), "0.5").expect("double");
    assert_eq!(cell(&core, &ctx, 1, "d"), Cell::Float(2.75));
    exec_add(&core, &ctx, &add_sql("r"), "0.5").expect("real");
    assert_eq!(cell(&core, &ctx, 1, "r"), Cell::Float(2.0));
}

#[test]
fn no_conflict_is_a_plain_insert() {
    let (core, path) = open_engine("sql49-addp-noconflict");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    exec_add(&core, &ctx, &add_sql("n"), "5").expect("insert");
    // VALUES の 0 がそのまま入り、加算は評価されない。
    assert_eq!(cell(&core, &ctx, 1, "n"), int(0));
}

#[test]
fn on_constraint_form_is_accepted_and_adds() {
    let path = unique_db_path("sql49-addp-onconstraint");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session_legacy(
        &ctx,
        &mut ddl,
        "CREATE TABLE cc (a INTEGER, n INTEGER, CONSTRAINT uq_a UNIQUE (a))",
    )
    .expect("create");
    let mut s = SessionState::default();
    core.execute_sql_in_session_legacy(
        &ctx,
        &mut s,
        "INSERT INTO cc (id, a, n) VALUES (1, 7, 10) USING OPERATION_ID 'cc-seed'",
    )
    .expect("seed");
    let sql = "INSERT INTO cc (id, a, n) VALUES (2, 7, 0) ON CONFLICT ON CONSTRAINT uq_a \
               DO UPDATE SET n = n + $1 RETURNING n USING OPERATION_ID 'cc-add'";
    let prepared = core.parse_sql_prepared(sql).expect("parse");
    assert!(matches!(
        prepared.param_types().first(),
        Some(PreparedParamType::Column(ColumnMeta::Scalar {
            ty: ColumnType::Integer,
            ..
        }))
    ));
    let parsed = core
        .bind_prepared(&prepared, &[Some(b"5".to_vec())])
        .expect("bind");
    let mut s = SessionState::default();
    core.execute_parsed_in_session(&ctx, &mut s, &parsed)
        .expect("execute");
    assert_eq!(cell_of(&core, &ctx, "cc", 1, "n"), int(15));
}

fn cell_of(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, col: &str) -> Cell {
    let sql = format!("SELECT {col} FROM {table} WHERE id = {id} LIMIT 10");
    let mut s = SessionState::default();
    let SqlOutcome::Query(result) = core
        .execute_sql_in_session_legacy(ctx, &mut s, &sql)
        .expect(&sql)
    else {
        panic!("expected Query outcome");
    };
    result.rows[0].cells[0].clone()
}

#[test]
fn omitted_column_list_resolves_the_type_from_the_table() {
    let (core, path) = open_engine("sql49-addp-omitted");
    let _g = CleanupGuard(path);
    let sql = format!(
        "INSERT INTO {TABLE} VALUES (1, '[0.1,0.2]', 0, 0, 0, 0, 0, 'x', 0) \
         ON CONFLICT (id) DO UPDATE SET big = big + $1 USING OPERATION_ID '{}'",
        op()
    );
    match slot_type(&core, &sql) {
        PreparedParamType::Column(ColumnMeta::Scalar { ty, .. }) => {
            assert_eq!(ty, ColumnType::BigInt)
        }
        other => panic!("unexpected slot type {other:?}"),
    }
}

#[test]
fn declared_type_mismatch_is_rejected_fail_closed() {
    let (core, path) = open_engine("sql49-addp-declared");
    let _g = CleanupGuard(path);
    let int_sql = add_sql("n");
    // 整数列: 整数系の宣言は受理、text／bool は `42883`、非整数の数値型は `0A000`。
    for d in [D::Int2, D::Int4, D::Int8] {
        assert_eq!(parse_code(&core, &int_sql, Some(d)), None);
    }
    assert_eq!(parse_code(&core, &int_sql, Some(D::Text)), Some("42883"));
    assert_eq!(parse_code(&core, &int_sql, Some(D::Bool)), Some("42883"));
    assert_eq!(parse_code(&core, &int_sql, Some(D::Float8)), Some("0A000"));
    assert_eq!(parse_code(&core, &int_sql, Some(D::Numeric)), Some("0A000"));
    // 浮動小数列: 数値系の宣言は受理、text は `42883`。
    let dbl_sql = add_sql("d");
    assert_eq!(parse_code(&core, &dbl_sql, Some(D::Float8)), None);
    assert_eq!(parse_code(&core, &dbl_sql, Some(D::Int4)), None);
    assert_eq!(parse_code(&core, &dbl_sql, Some(D::Text)), Some("42883"));
}

#[test]
fn non_numeric_target_columns_use_the_literal_path_codes() {
    let (core, path) = open_engine("sql49-addp-nonnumeric");
    let _g = CleanupGuard(path);
    assert_eq!(parse_code(&core, &add_sql("t"), None), Some("42883"));
    assert_eq!(parse_code(&core, &add_sql("num"), None), Some("0A000"));
}

#[test]
fn invalid_and_out_of_range_values_fail_without_side_effects() {
    let (core, path) = open_engine("sql49-addp-values");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "n", &i32::MAX.to_string());
    let sql = add_sql("n");
    for bad in ["abc", "1.5", "+5", ""] {
        assert_eq!(
            exec_add(&core, &ctx, &sql, bad).unwrap_err(),
            "22P02",
            "{bad:?}"
        );
    }
    // 和が INTEGER の範囲外 → `22003` で行は不変。
    assert_eq!(exec_add(&core, &ctx, &sql, "1").unwrap_err(), "22003");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(i64::from(i32::MAX)));
}

#[test]
fn null_bind_is_rejected_like_the_null_literal() {
    let (core, path) = open_engine("sql49-addp-null");
    let _g = CleanupGuard(path);
    let sql = add_sql("n");
    let prepared = core.parse_sql_prepared(&sql).expect("parse");
    let err = core.bind_prepared(&prepared, &[None]).unwrap_err();
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn unsupported_param_shapes_stay_rejected() {
    let (core, path) = open_engine("sql49-addp-reject");
    let _g = CleanupGuard(path);
    for set in [
        "n = $1",
        "n = $1 + n",
        "n = big + $1",
        "n = n + $1 + 1",
        "n = n + EXCLUDED.n + $1",
        "n = n + $1::int",
    ] {
        let sql = format!(
            "INSERT INTO {TABLE} (id, embedding, n, big) VALUES (1, '[0.1,0.2]', 0, 0) \
             ON CONFLICT (id) DO UPDATE SET {set} USING OPERATION_ID 'x'"
        );
        assert_eq!(parse_code(&core, &sql, None), Some("42601"), "{set}");
    }
}

#[test]
fn ledger_resend_is_matched_on_the_substituted_statement() {
    let (core, path) = open_engine("sql49-addp-ledger");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "n", "10");
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, n) VALUES (1, '[0.1,0.2]', 0) \
         ON CONFLICT (id) DO UPDATE SET n = n + $1 USING OPERATION_ID 'addp-resend'"
    );
    exec_add(&core, &ctx, &sql, "5").expect("first");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(15));
    // 同じ値の再送は内容一致（再実行されない）、異なる値は内容不一致 `22023`。
    let same = exec_add(&core, &ctx, &sql, "5");
    assert!(same.is_ok() || same == Err("23505"), "{same:?}");
    assert_eq!(exec_add(&core, &ctx, &sql, "6").unwrap_err(), "22023");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(15));
}

#[test]
fn other_tenants_rows_are_not_added_to() {
    let (core, path) = open_engine("sql49-addp-tenant");
    let _g = CleanupGuard(path);
    let alice = ctx_for("tenant-a");
    let bob = ctx_for("tenant-b");
    let carol = ctx_for("tenant-c");
    seed(&core, &alice, 1, "n", "10");
    seed(&core, &bob, 1, "n", "100");
    seed(&core, &carol, 1, "n", "1000");
    exec_add(&core, &alice, &add_sql("n"), "5").expect("alice");
    assert_eq!(cell(&core, &alice, 1, "n"), int(15));
    assert_eq!(cell(&core, &bob, 1, "n"), int(100));
    assert_eq!(cell(&core, &carol, 1, "n"), int(1000));
}
