//! `INSERT ... ON CONFLICT ... DO UPDATE SET <col> = <col> + (<数値リテラル> | EXCLUDED.<col>)`
//! （SQL-20・SQL-49・RECOVER-11、Issue #2425）の結合テスト。
//!
//! ポインタ: `docs/spec/04-behavior/` の SQL-20・SQL-49・RECOVER-11。`EngineCore::
//! execute_sql_in_session`（parser → `bind_upsert_assignments` → `tenant::upsert_typed_rows_unchecked`
//! の read-merge-write）を production 経路として検証する。期待値は PostgreSQL 17 の結果
//! （`INSERT ... ON CONFLICT DO UPDATE SET n = t.n + ...`）に合わせてある。
//! `$n` の受理（#2427）・並行 64 本の加算（#2428）は本テストの対象外。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "counters";

static OP_SEQ: AtomicU64 = AtomicU64::new(0);

fn op() -> String {
    format!("op-add-{}", OP_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("nn", ColumnType::Integer, false),
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

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    run(core, ctx, sql).expect_err(sql).wire_code()
}

fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, cols: &str, vals: &str) {
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, {cols}) VALUES ({id}, '[0.1,0.2]', 0, {vals}) \
         USING OPERATION_ID '{}'",
        op()
    );
    run(core, ctx, &sql).expect(&sql);
}

/// `ON CONFLICT (id) DO UPDATE SET <set>` を 1 行の VALUES で送る。
fn upsert(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
    cols: &str,
    vals: &str,
    set: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, {cols}) VALUES ({id}, '[0.1,0.2]', 0, {vals}) \
         ON CONFLICT (id) DO UPDATE SET {set} USING OPERATION_ID '{}'",
        op()
    );
    run(core, ctx, &sql)
}

fn cell(core: &EngineCore, ctx: &PolicyContext, id: u64, col: &str) -> Cell {
    let sql = format!("SELECT {col} FROM {TABLE} WHERE id = {id} LIMIT 10");
    let SqlOutcome::Query(result) = run(core, ctx, &sql).expect(&sql) else {
        panic!("expected Query outcome");
    };
    assert_eq!(result.rows.len(), 1, "{sql}");
    result.rows[0].cells[0].clone()
}

fn row_count(core: &EngineCore, ctx: &PolicyContext) -> usize {
    let sql = format!("SELECT n FROM {TABLE} LIMIT 1000");
    let SqlOutcome::Query(result) = run(core, ctx, &sql).expect(&sql) else {
        panic!("expected Query outcome");
    };
    result.rows.len()
}

fn int(v: i64) -> Cell {
    Cell::SignedInteger(v)
}

#[test]
fn integer_add_literal_and_excluded_match_pg17() {
    let (core, path) = open_engine("sql49-add-basic");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", "10");
    // PG17: n = 10 + 5 = 15
    upsert(&core, &ctx, 1, "n", "1", "n = n + 5").expect("add");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(15));
    // PG17: 15 + (-3) = 12
    upsert(&core, &ctx, 1, "n", "1", "n = n + -3").expect("add negative");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(12));
    // PG17: 12 + EXCLUDED.n(7) = 19
    upsert(&core, &ctx, 1, "n", "7", "n = n + EXCLUDED.n").expect("add excluded");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(19));
}

#[test]
fn bigint_beyond_2_pow_53_is_exact_and_wider_operand_is_accepted() {
    let (core, path) = open_engine("sql49-add-bigint");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let base: i64 = (1_i64 << 53) + 1;
    insert(&core, &ctx, 1, "big, n", &format!("{base}, 100"));
    // PG17: bigint + 1 は厳密（2^53 + 2）
    upsert(&core, &ctx, 1, "big", "1", "big = big + 1").expect("bigint add");
    assert_eq!(cell(&core, &ctx, 1, "big"), int(base + 1));
    // INTEGER 代入先 + EXCLUDED.<BIGINT>（結果が範囲内）。PG17: 100 + (-50) = 50
    upsert(&core, &ctx, 1, "n, big", "0, -50", "n = n + EXCLUDED.big").expect("narrow target");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(50));
}

#[test]
fn smallint_real_double_add() {
    let (core, path) = open_engine("sql49-add-small-float");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "sm, r, d", "100, 1.5, 2.25");
    upsert(&core, &ctx, 1, "sm", "1", "sm = sm + 7").expect("smallint");
    assert_eq!(cell(&core, &ctx, 1, "sm"), int(107));
    upsert(&core, &ctx, 1, "r", "1", "r = r + 0.5").expect("real");
    assert_eq!(cell(&core, &ctx, 1, "r"), Cell::Float(2.0));
    upsert(&core, &ctx, 1, "d", "1", "d = d + 0.5").expect("double");
    assert_eq!(cell(&core, &ctx, 1, "d"), Cell::Float(2.75));
    // 浮動小数の代入先には整数オペランド（EXCLUDED.<INTEGER>）も可
    upsert(&core, &ctx, 1, "d, n", "0, 2", "d = d + EXCLUDED.n").expect("double + int");
    assert_eq!(cell(&core, &ctx, 1, "d"), Cell::Float(4.75));
}

#[test]
fn non_conflicting_row_is_plain_insert_and_add_is_not_evaluated() {
    let (core, path) = open_engine("sql49-add-no-conflict");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    // 衝突しない場合は VALUES どおり挿入（既存値が無いので NULL 加算も 23502 も起きない）
    upsert(&core, &ctx, 1, "n", "5", "n = n + 1").expect("insert");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(5));
}

#[test]
fn multiple_assignments_and_returning_use_post_update_values() {
    let (core, path) = open_engine("sql49-add-multi-returning");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n, big, t", "1, 2, 'x'");
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n, big, t) VALUES (1, '[0.1,0.2]', 0, 9, 9, 'y') \
         ON CONFLICT (id) DO UPDATE SET n = n + 10, big = big + EXCLUDED.big, t = EXCLUDED.t \
         RETURNING n, big, t USING OPERATION_ID '{}'",
        op()
    );
    let SqlOutcome::Returning(o) = run(&core, &ctx, &sql).expect(&sql) else {
        panic!("expected Returning outcome");
    };
    let rows = &o.result.rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cells[0], int(11));
    assert_eq!(rows[0].cells[1], int(11));
    assert_eq!(rows[0].cells[2], Cell::Text("y".to_string()));
}

#[test]
fn null_semantics() {
    let (core, path) = open_engine("sql49-add-null");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    // 既存値 NULL（nullable）→ NULL のまま（PG17 と同じ）
    insert(&core, &ctx, 1, "t", "'x'");
    upsert(&core, &ctx, 1, "n", "1", "n = n + 1").expect("null + 1");
    assert_eq!(cell(&core, &ctx, 1, "n"), Cell::Null);
    // NOT NULL 列の既存値 + EXCLUDED が NULL（列省略）→ 23502
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn) VALUES (1, '[0.1,0.2]', 3) \
         ON CONFLICT (id) DO UPDATE SET nn = nn + EXCLUDED.n USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &ctx, &sql), "23502");
    // 衝突しなければ 23502 にならない
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn) VALUES (2, '[0.1,0.2]', 3) \
         ON CONFLICT (id) DO UPDATE SET nn = nn + EXCLUDED.n USING OPERATION_ID '{}'",
        op()
    );
    run(&core, &ctx, &sql).expect("no conflict");
}

#[test]
fn out_of_range_is_22003_and_leaves_no_side_effect() {
    let (core, path) = open_engine("sql49-add-range");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(
        &core,
        &ctx,
        1,
        "n, big, sm, r",
        &format!("{}, {}, 32767, 3.0e38", i32::MAX, i64::MAX),
    );
    for set in [
        "n = n + 1",
        "big = big + 1",
        "sm = sm + 1",
        "r = r + 3.0e38",
    ] {
        let col = set.split(' ').next().unwrap();
        assert_eq!(
            upsert(&core, &ctx, 1, col, "1", set)
                .unwrap_err()
                .wire_code(),
            "22003",
            "{set}"
        );
    }
    assert_eq!(cell(&core, &ctx, 1, "n"), int(i64::from(i32::MAX)));
    assert_eq!(cell(&core, &ctx, 1, "big"), int(i64::MAX));
    assert_eq!(cell(&core, &ctx, 1, "sm"), int(32767));
    assert_eq!(row_count(&core, &ctx), 1);

    // 同じ operation_id で範囲内の文を再送できる（失敗時に台帳へ何も残っていない）
    let op_id = "op-add-range-retry";
    let operation_id = engine::recovery::required_op_id::OperationId::parse(op_id).unwrap();
    let bad = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES (1, '[0.1,0.2]', 0, 1) \
         ON CONFLICT (id) DO UPDATE SET n = n + 1 USING OPERATION_ID '{op_id}'"
    );
    assert_eq!(code(&core, &ctx, &bad), "22003");
    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &operation_id).unwrap(),
        LedgerLookup::NotRecorded
    );
    let ok = bad.replace("n = n + 1", "n = n + -1");
    run(&core, &ctx, &ok).expect("retry with in-range statement");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(i64::from(i32::MAX) - 1));
}

#[test]
fn multi_row_failure_rolls_back_earlier_rows() {
    let (core, path) = open_engine("sql49-add-multirow");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", "1");
    insert(&core, &ctx, 2, "n", &i32::MAX.to_string());
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES \
         (1, '[0.1,0.2]', 0, 1), (2, '[0.1,0.2]', 0, 1), (3, '[0.1,0.2]', 0, 1) \
         ON CONFLICT (id) DO UPDATE SET n = n + 1 USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &ctx, &sql), "22003");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(1));
    assert_eq!(cell(&core, &ctx, 2, "n"), int(i64::from(i32::MAX)));
    assert_eq!(row_count(&core, &ctx), 2);
}

#[test]
fn ledger_content_check_uses_statement_shape_not_result() {
    let (core, path) = open_engine("sql49-add-ledger");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", "10");
    let mk = |set: &str, op_id: &str| {
        format!(
            "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES (1, '[0.1,0.2]', 0, 1) \
             ON CONFLICT (id) DO UPDATE SET {set} USING OPERATION_ID '{op_id}'"
        )
    };
    let first = mk("n = n + 1", "op-add-ledger-1");
    run(&core, &ctx, &first).expect("first");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(11));
    // 同一 operation_id・同一文の再送は二重加算されない
    assert_eq!(code(&core, &ctx, &first), "23505");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(11));
    // リテラルだけ違う文・別形（n = 1）は内容不一致
    assert_eq!(
        code(&core, &ctx, &mk("n = n + 2", "op-add-ledger-1")),
        "22023"
    );
    assert_eq!(code(&core, &ctx, &mk("n = 1", "op-add-ledger-1")), "22023");
    assert_eq!(
        code(&core, &ctx, &mk("n = n + EXCLUDED.n", "op-add-ledger-1")),
        "22023"
    );
    assert_eq!(cell(&core, &ctx, 1, "n"), int(11));
}

#[test]
fn unsupported_shapes_are_rejected_fail_closed() {
    let (core, path) = open_engine("sql49-add-reject");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n, d, t, num", "1, 1.0, 'x', 1.00");
    // 加算形に当てはまらない式（別列・左右逆・他演算子・連鎖）は Issue #1739 で一般の式として
    // 受理するため、ここ（厳密な加算形の fail-closed 検証）には含めない。
    // 受理側の挙動は `sql49_upsert_set_expr.rs`。
    let cases: [(&str, &str); 11] = [
        ("n = n + NULL", "42601"),
        ("n = n + 'x'", "42601"),
        ("n = n + true", "42601"),
        ("num = num + 1", "0A000"),
        ("n = n + 1.5", "0A000"),
        ("n = n + EXCLUDED.d", "0A000"),
        ("t = t + 1", "42883"),
        ("n = n + EXCLUDED.t", "42883"),
        ("id = id + 1", "42601"),
        ("tenant_id = tenant_id + 1", "42601"),
        ("zzz = zzz + 1", "42703"),
    ];
    for (set, expected) in cases {
        let got = upsert(&core, &ctx, 1, "n, d", "1, 1.0", set)
            .expect_err(set)
            .wire_code();
        assert_eq!(got, expected, "{set}");
    }
    assert_eq!(cell(&core, &ctx, 1, "n"), int(1));
}

#[test]
fn other_tenant_row_never_triggers_evaluation() {
    let (core, path) = open_engine("sql49-add-tenant");
    let _g = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    insert(&core, &b, 1, "n", &i32::MAX.to_string());
    // tenant-a から見て id=1 は衝突しない（新規挿入）。22003 で他テナントの値を漏らさない。
    upsert(&core, &a, 1, "n", "1", "n = n + 1").expect("fresh insert for tenant-a");
    assert_eq!(cell(&core, &a, 1, "n"), int(1));
    assert_eq!(cell(&core, &b, 1, "n"), int(i64::from(i32::MAX)));
}

#[test]
fn explicit_transaction_sees_same_result() {
    let (core, path) = open_engine("sql49-add-txn");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", "1");
    let mut txn = core.new_session_transaction();
    let mut session = SessionState::default();
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    for _ in 0..2 {
        let sql = format!(
            "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES (1, '[0.1,0.2]', 0, 1) \
             ON CONFLICT (id) DO UPDATE SET n = n + 1 USING OPERATION_ID '{}'",
            op()
        );
        core.execute_sql_in_txn(&ctx, &mut session, &mut txn, &sql)
            .expect(&sql);
    }
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(3));
}
