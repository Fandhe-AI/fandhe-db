//! `CHECK` 制約が参照する列への `ALTER COLUMN TYPE` の拡大変換
//! （`INTEGER`→`BIGINT`・`REAL`→`DOUBLE PRECISION`。TABLE-19・TABLE-16・SQL-23・ERR-1・
//! ERR-2・ERR-6、Issue #1427）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-19・TABLE-16。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、拡大後も CHECK の判定結果
//! （`23514`・式評価エラーの `wire_code`）が変わらないこと、縮小・非互換は `42804` のまま
//! であること、永続化後も CHECK が強制されることを確認する。

use engine::catalog::ColumnType;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn exec(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = ddl_session();
    core.execute_sql_in_session(ctx, &mut s, sql)
}

fn ok(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    exec(core, ctx, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    exec(core, ctx, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

fn ints(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<i64> {
    let mut v: Vec<i64> = core
        .execute_sql(ctx, sql)
        .expect("select")
        .rows
        .into_iter()
        .map(|r| match r.cells.into_iter().next().expect("cell") {
            Cell::SignedInteger(v) => v,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn col_type(storage: &Storage, table: &str, col: &str) -> ColumnType {
    storage
        .get_table_schema(table)
        .expect("schema")
        .columns
        .iter()
        .find(|c| c.name == col)
        .expect("column")
        .ty
        .clone()
}

const WIDE: i64 = 3_000_000_000;

/// 列制約 CHECK の INTEGER 列を拡大でき、全テナントの既存値が不変で、拡大の前後で
/// 同じ書き込みが同じ `23514` になる。i32 範囲外の新値は拡大後に受理される。
#[test]
fn widening_check_column_is_accepted_and_check_still_enforced() {
    let (core, path) = new_core("ackc-basic");
    let _g = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    ok(
        &core,
        &alice,
        "CREATE TABLE t (qty INTEGER CONSTRAINT qty_pos CHECK (qty > 0), note TEXT)",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO t (id, qty, note) VALUES (1, 5, 'a') USING OPERATION_ID 'a1'",
    );
    ok(
        &core,
        &bob,
        "INSERT INTO t (id, qty, note) VALUES (2, 7, 'b') USING OPERATION_ID 'b1'",
    );
    let bad = "INSERT INTO t (id, qty) VALUES (9, 0) USING OPERATION_ID 'bad'";
    let before = exec(&core, &alice, bad).expect_err("violation before");
    assert_eq!(before.wire_code(), "23514");

    ok(&core, &alice, "ALTER TABLE t ALTER COLUMN qty TYPE BIGINT");

    let after = exec(&core, &alice, bad).expect_err("violation after");
    assert_eq!(after.wire_code(), "23514");
    assert_eq!(format!("{before:?}"), format!("{after:?}"));
    assert_eq!(
        code(
            &core,
            &alice,
            "UPDATE t SET qty = 0 WHERE id = 1 USING OPERATION_ID 'u0'"
        ),
        "23514"
    );
    assert_eq!(ints(&core, &alice, "SELECT qty FROM t LIMIT 10"), vec![5]);
    assert_eq!(ints(&core, &bob, "SELECT qty FROM t LIMIT 10"), vec![7]);
    // 拡大後は i32 範囲外の正の値を受理する。
    ok(
        &core,
        &alice,
        &format!("INSERT INTO t (id, qty) VALUES (3, {WIDE}) USING OPERATION_ID 'w'"),
    );
    // CHECK 非参照列の更新は行全体の CHECK が再評価されて成功する。
    ok(
        &core,
        &alice,
        "UPDATE t SET note = 'z' WHERE id = 1 USING OPERATION_ID 'un'",
    );
}

/// 式評価エラー（0 除算 `22012`）の `wire_code` は拡大の前後で変わらない。
#[test]
fn expression_evaluation_error_code_is_unchanged_by_widening() {
    let (core, path) = new_core("ackc-expr");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(
        &core,
        &o,
        "CREATE TABLE t (qty INTEGER CHECK (10 / (qty - 5) > 0))",
    );
    ok(
        &core,
        &o,
        "INSERT INTO t (id, qty) VALUES (1, 6) USING OPERATION_ID 'a1'",
    );
    let sql = "INSERT INTO t (id, qty) VALUES (2, 5) USING OPERATION_ID 'z'";
    let before = code(&core, &o, sql);
    assert_eq!(before, "22012");
    ok(&core, &o, "ALTER TABLE t ALTER COLUMN qty TYPE BIGINT");
    assert_eq!(code(&core, &o, sql), before);
    assert_eq!(ints(&core, &o, "SELECT qty FROM t LIMIT 10"), vec![6]);
}

/// 複数列の表制約（INTEGER 列と BIGINT 列）でも片側の拡大後に判定が変わらない。
#[test]
fn table_check_over_multiple_columns_survives_widening() {
    let (core, path) = new_core("ackc-multi");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(
        &core,
        &o,
        "CREATE TABLE t (a INTEGER, b BIGINT, CONSTRAINT ab CHECK (a < b))",
    );
    ok(
        &core,
        &o,
        "INSERT INTO t (id, a, b) VALUES (1, 1, 2) USING OPERATION_ID 'a1'",
    );
    ok(&core, &o, "ALTER TABLE t ALTER COLUMN a TYPE BIGINT");
    assert_eq!(
        code(
            &core,
            &o,
            "INSERT INTO t (id, a, b) VALUES (2, 5, 5) USING OPERATION_ID 'x'"
        ),
        "23514"
    );
    ok(
        &core,
        &o,
        "INSERT INTO t (id, a, b) VALUES (3, 1, 9) USING OPERATION_ID 'y'",
    );
}

/// REAL→DOUBLE PRECISION: 既存値は無損失で、f32 境界値の CHECK 判定が拡大の前後で一致する。
#[test]
fn real_to_double_keeps_check_outcome() {
    let (core, path) = new_core("ackc-real");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(&core, &o, "CREATE TABLE t (note TEXT)");
    ok(&core, &o, "ALTER TABLE t ADD COLUMN r REAL");
    ok(
        &core,
        &o,
        "ALTER TABLE t ADD CONSTRAINT r_ck CHECK (r > 0.5)",
    );
    ok(
        &core,
        &o,
        "INSERT INTO t (id, r) VALUES (1, 0.75) USING OPERATION_ID 'a1'",
    );
    let upd = "UPDATE t SET note = 'x' WHERE id = 1 USING OPERATION_ID 'u1'";
    let upd2 = "UPDATE t SET note = 'y' WHERE id = 1 USING OPERATION_ID 'u2'";
    let viol = "INSERT INTO t (id, r) VALUES (2, 0.25) USING OPERATION_ID 'v'";
    ok(&core, &o, upd);
    let before = code(&core, &o, viol);
    assert_eq!(before, "23514");
    ok(
        &core,
        &o,
        "ALTER TABLE t ALTER COLUMN r TYPE DOUBLE PRECISION",
    );
    ok(&core, &o, upd2);
    assert_eq!(code(&core, &o, viol), before);
    let rows = core
        .execute_sql(&o, "SELECT r FROM t LIMIT 10")
        .expect("select");
    assert!(matches!(rows.rows[0].cells[0], Cell::Float(f) if f == 0.75));
}

/// 縮小・同一型・非互換は CHECK 参照列でも `42804`、状態は不変。
#[test]
fn non_widening_on_check_column_is_42804_and_changes_nothing() {
    let (core, path) = new_core("ackc-reject");
    let _g = CleanupGuard(path);
    let o = ctx("owner");
    ok(
        &core,
        &o,
        "CREATE TABLE t (a INTEGER CHECK (a > 0), b BIGINT CHECK (b > 0))",
    );
    ok(
        &core,
        &o,
        "INSERT INTO t (id, a, b) VALUES (1, 1, 1) USING OPERATION_ID 'a1'",
    );
    for sql in [
        "ALTER TABLE t ALTER COLUMN a TYPE TEXT",
        "ALTER TABLE t ALTER COLUMN b TYPE INTEGER",
        "ALTER TABLE t ALTER COLUMN b TYPE BIGINT",
        "ALTER TABLE t ALTER COLUMN a TYPE DOUBLE PRECISION",
    ] {
        assert_eq!(code(&core, &o, sql), "42804", "{sql}");
    }
    assert_eq!(ints(&core, &o, "SELECT a FROM t LIMIT 10"), vec![1]);
    assert_eq!(
        code(
            &core,
            &o,
            "INSERT INTO t (id, a, b) VALUES (2, 0, 1) USING OPERATION_ID 'x'"
        ),
        "23514"
    );
}

/// 拡大後に開き直しても型は BIGINT で CHECK は強制される。PK／UNIQUE と CHECK を併せ持つ列でも
/// 拡大でき、一意性違反 `23505` と `23514` が機能する。
#[test]
fn widening_persists_and_works_with_primary_key() {
    let (core, path) = new_core("ackc-persist");
    let _g = CleanupGuard(path.clone());
    let o = ctx("owner");
    ok(
        &core,
        &o,
        "CREATE TABLE t (code INTEGER PRIMARY KEY CHECK (code > 0))",
    );
    ok(
        &core,
        &o,
        "INSERT INTO t (id, code) VALUES (1, 1) USING OPERATION_ID 'a1'",
    );
    ok(&core, &o, "ALTER TABLE t ALTER COLUMN code TYPE BIGINT");
    drop(core);

    let storage = Storage::open(&path).expect("reopen");
    assert_eq!(col_type(&storage, "t", "code"), ColumnType::BigInt);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        code(
            &core,
            &o,
            "INSERT INTO t (id, code) VALUES (2, 1) USING OPERATION_ID 'd'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            &o,
            "INSERT INTO t (id, code) VALUES (3, 0) USING OPERATION_ID 'c'"
        ),
        "23514"
    );
    ok(
        &core,
        &o,
        &format!("INSERT INTO t (id, code) VALUES (4, {WIDE}) USING OPERATION_ID 'w'"),
    );
}
