//! `INSERT ... ON CONFLICT ... DO UPDATE SET <col> = <一般の値式>`（SQL-20・SQL-49・
//! RECOVER-11、Issue #1739）の結合テスト。
//!
//! ポインタ: `docs/spec/04-behavior/` の SQL-20・SQL-49・RECOVER-11。`EngineCore::
//! execute_sql_in_session`（parser → `bind_upsert_assignments` → `sql::upsert_expr` →
//! `tenant::upsert_typed_rows_unchecked` の read-merge-write）を production 経路として検証する。
//! 期待値は PostgreSQL 17 の結果（`INSERT ... ON CONFLICT DO UPDATE SET n = t.n * 2 + 1` 等）に
//! 合わせてある。厳密な加算形（`n = n + 1`）は `sql49_upsert_add_expr.rs`、`$n` は
//! `sql49_upsert_add_param.rs` が担う。

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
    format!("op-setexpr-{}", OP_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("m", ColumnType::Integer, true),
            ColumnDef::new("nn", ColumnType::Integer, false),
            ColumnDef::new("sm", ColumnType::SmallInt, true),
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

fn text(v: &str) -> Cell {
    Cell::Text(v.to_string())
}

#[test]
fn arithmetic_over_existing_and_excluded_columns_matches_pg17() {
    let (core, path) = open_engine("sql49-expr-arith");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", "10");
    // PG17: n = counters.n + EXCLUDED.n = 10 + 7
    upsert(&core, &ctx, 1, "n", "7", "n = counters.n + EXCLUDED.n").expect("qualified");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(17));
    // PG17: n * 2 + 1 = 35
    upsert(&core, &ctx, 1, "n", "0", "n = n * 2 + 1").expect("mul add");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(35));
    // PG17: 1 + n（左右逆）と n - 5
    upsert(&core, &ctx, 1, "n", "0", "n = 1 + n").expect("reversed");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(36));
    upsert(&core, &ctx, 1, "n", "0", "n = n - 6").expect("sub");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(30));
    // PG17: EXCLUDED.n + 1 は既存値を使わない
    upsert(&core, &ctx, 1, "n", "4", "n = EXCLUDED.n + 1").expect("excluded only");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(5));
    // 加算形の続きがある式（n + 1 * 2）は一般の式として評価する。PG17: 5 + 2 = 7
    upsert(&core, &ctx, 1, "n", "0", "n = n + 1 * 2").expect("precedence");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(7));
    // 括弧
    upsert(&core, &ctx, 1, "n", "0", "n = (n + 1) * 3").expect("paren");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(24));
}

#[test]
fn integer_division_truncates_and_double_arithmetic() {
    let (core, path) = open_engine("sql49-expr-div");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n, d", "7, 4.0");
    // PG17: 7 / 2 = 3（整数除算）
    upsert(&core, &ctx, 1, "n", "0", "n = n / 2").expect("int div");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(3));
    // PG17: d * 1.5 = 6.0
    upsert(&core, &ctx, 1, "d", "0", "d = d * 1.5").expect("double");
    assert_eq!(cell(&core, &ctx, 1, "d"), Cell::Float(6.0));
}

#[test]
fn text_functions_case_and_coalesce() {
    let (core, path) = open_engine("sql49-expr-text");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n, t", "5, 'ab'");
    // PG17: concat(upper(EXCLUDED.t), '!') → 'XY!'
    upsert(
        &core,
        &ctx,
        1,
        "t",
        "'xy'",
        "t = concat(upper(EXCLUDED.t), '!')",
    )
    .expect("text");
    assert_eq!(cell(&core, &ctx, 1, "t"), text("XY!"));
    // CASE: EXCLUDED.n(9) > n(5) なら EXCLUDED.n、でなければ n
    upsert(
        &core,
        &ctx,
        1,
        "n",
        "9",
        "n = CASE WHEN EXCLUDED.n > n THEN EXCLUDED.n ELSE n END",
    )
    .expect("case");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(9));
    upsert(
        &core,
        &ctx,
        1,
        "n",
        "3",
        "n = CASE WHEN EXCLUDED.n > n THEN EXCLUDED.n ELSE n END",
    )
    .expect("case keeps");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(9));
    // COALESCE: m は NULL → 0 + 1。PG17: 1
    upsert(&core, &ctx, 1, "m", "0", "m = COALESCE(m, 0) + 1").expect("coalesce");
    assert_eq!(cell(&core, &ctx, 1, "m"), int(1));
    // NULL 伝播: 既存 m が NULL の行の m * 2 は NULL（PG17 と同じ）
    insert(&core, &ctx, 2, "n", "1");
    upsert(&core, &ctx, 2, "m", "0", "m = m * 2").expect("null propagation");
    assert_eq!(cell(&core, &ctx, 2, "m"), Cell::Null);
}

#[test]
fn all_assignments_see_the_pre_update_row() {
    let (core, path) = open_engine("sql49-expr-swap");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n, m", "1, 2");
    // PG17: SET n = m, m = n は更新前の行を見る（入れ替え）
    upsert(&core, &ctx, 1, "n", "0", "n = m, m = n").expect("swap");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(2));
    assert_eq!(cell(&core, &ctx, 1, "m"), int(1));
    // 式と厳密加算形・リテラルの混在
    upsert(&core, &ctx, 1, "n", "9", "n = n + 1, m = m * 10, t = 'z'").expect("mixed");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(3));
    assert_eq!(cell(&core, &ctx, 1, "m"), int(10));
    assert_eq!(cell(&core, &ctx, 1, "t"), text("z"));
}

#[test]
fn multi_row_values_use_per_row_excluded_and_skip_non_conflicting() {
    let (core, path) = open_engine("sql49-expr-multirow");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", "10");
    insert(&core, &ctx, 2, "n", "20");
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES \
         (1, '[0.1,0.2]', 0, 1), (2, '[0.1,0.2]', 0, 2), (3, '[0.1,0.2]', 0, 3) \
         ON CONFLICT (id) DO UPDATE SET n = n * 100 + EXCLUDED.n USING OPERATION_ID '{}'",
        op()
    );
    let SqlOutcome::Insert(o) = run(&core, &ctx, &sql).expect(&sql) else {
        panic!("expected insert outcome");
    };
    // 影響行数は挿入 1 + 更新 2（PG17 の INSERT 0 3）
    assert_eq!(o.rows_affected, 3);
    assert_eq!(cell(&core, &ctx, 1, "n"), int(1001));
    assert_eq!(cell(&core, &ctx, 2, "n"), int(2002));
    // 衝突しない行はただの挿入（式は評価されない）
    assert_eq!(cell(&core, &ctx, 3, "n"), int(3));
}

#[test]
fn returning_reports_post_update_values() {
    let (core, path) = open_engine("sql49-expr-returning");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n, t", "4, 'a'");
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n, t) VALUES (1, '[0.1,0.2]', 0, 6, 'b') \
         ON CONFLICT (id) DO UPDATE SET n = n * EXCLUDED.n, t = concat(t, EXCLUDED.t) \
         RETURNING n, t USING OPERATION_ID '{}'",
        op()
    );
    let SqlOutcome::Returning(o) = run(&core, &ctx, &sql).expect(&sql) else {
        panic!("expected Returning outcome");
    };
    let rows = &o.result.rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cells[0], int(24));
    assert_eq!(rows[0].cells[1], text("ab"));
}

#[test]
fn out_of_range_and_division_by_zero_leave_no_side_effect() {
    let (core, path) = open_engine("sql49-expr-range");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n, sm", &format!("{}, 32767", i32::MAX));
    for (set, col) in [("n = n * 2", "n"), ("sm = sm * 2", "sm")] {
        assert_eq!(
            upsert(&core, &ctx, 1, col, "1", set)
                .unwrap_err()
                .wire_code(),
            "22003",
            "{set}"
        );
    }
    assert_eq!(
        upsert(&core, &ctx, 1, "n", "0", "n = n / EXCLUDED.n")
            .unwrap_err()
            .wire_code(),
        "22012"
    );
    assert_eq!(cell(&core, &ctx, 1, "n"), int(i64::from(i32::MAX)));
    assert_eq!(cell(&core, &ctx, 1, "sm"), int(32767));
    assert_eq!(row_count(&core, &ctx), 1);
}

#[test]
fn multi_row_failure_rolls_back_earlier_rows() {
    let (core, path) = open_engine("sql49-expr-rollback");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", "1");
    insert(&core, &ctx, 2, "n", &i32::MAX.to_string());
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES \
         (1, '[0.1,0.2]', 0, 1), (2, '[0.1,0.2]', 0, 1), (3, '[0.1,0.2]', 0, 1) \
         ON CONFLICT (id) DO UPDATE SET n = n * 2 USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &ctx, &sql), "22003");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(1));
    assert_eq!(cell(&core, &ctx, 2, "n"), int(i64::from(i32::MAX)));
    assert_eq!(row_count(&core, &ctx), 2);
}

#[test]
fn null_result_into_not_null_column_is_23502() {
    let (core, path) = open_engine("sql49-expr-notnull");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", "1");
    // EXCLUDED.m は列省略で NULL → nn + NULL = NULL → NOT NULL 列 nn へ
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn) VALUES (1, '[0.1,0.2]', 3) \
         ON CONFLICT (id) DO UPDATE SET nn = nn + EXCLUDED.m * 1 USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &ctx, &sql), "23502");
    // 衝突しなければ評価されず 23502 にならない
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn) VALUES (2, '[0.1,0.2]', 3) \
         ON CONFLICT (id) DO UPDATE SET nn = nn + EXCLUDED.m * 1 USING OPERATION_ID '{}'",
        op()
    );
    run(&core, &ctx, &sql).expect("no conflict");
}

#[test]
fn ledger_content_check_distinguishes_expressions() {
    let (core, path) = open_engine("sql49-expr-ledger");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", "10");
    let mk = |set: &str, op_id: &str| {
        format!(
            "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES (1, '[0.1,0.2]', 0, 1) \
             ON CONFLICT (id) DO UPDATE SET {set} USING OPERATION_ID '{op_id}'"
        )
    };
    let first = mk("n = n * 2", "op-setexpr-ledger-1");
    run(&core, &ctx, &first).expect("first");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(20));
    // 同一 operation_id・同一文の再送は二重評価されない
    assert_eq!(code(&core, &ctx, &first), "23505");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(20));
    // 式が違う再送・別形は内容不一致
    for other in [
        "n = n * 3",
        "n = n + 2",
        "n = m * 2",
        "n = EXCLUDED.n * 2",
        "n = n / 2",
        "n = n / 2.0",
        "n = 20",
    ] {
        assert_eq!(
            code(&core, &ctx, &mk(other, "op-setexpr-ledger-1")),
            "22023",
            "{other}"
        );
    }
    assert_eq!(cell(&core, &ctx, 1, "n"), int(20));
    // 表名修飾の有無は同じ文の形（`counters.n` と `n` は同一）
    let q1 = mk("n = counters.n * 2", "op-setexpr-ledger-2");
    run(&core, &ctx, &q1).expect("qualified first");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(40));
    assert_eq!(
        code(&core, &ctx, &mk("n = n * 2", "op-setexpr-ledger-2")),
        "23505"
    );
    assert_eq!(cell(&core, &ctx, 1, "n"), int(40));
}

#[test]
fn failed_statement_leaves_no_ledger_entry() {
    let (core, path) = open_engine("sql49-expr-ledger-fail");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n", &i32::MAX.to_string());
    let op_id = "op-setexpr-retry";
    let operation_id = engine::recovery::required_op_id::OperationId::parse(op_id).unwrap();
    let bad = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES (1, '[0.1,0.2]', 0, 1) \
         ON CONFLICT (id) DO UPDATE SET n = n * 2 USING OPERATION_ID '{op_id}'"
    );
    assert_eq!(code(&core, &ctx, &bad), "22003");
    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &operation_id).unwrap(),
        LedgerLookup::NotRecorded
    );
    let ok = bad.replace("n = n * 2", "n = n / 2");
    run(&core, &ctx, &ok).expect("retry with in-range statement");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(i64::from(i32::MAX) / 2));
}

#[test]
fn rejected_shapes_use_stable_codes() {
    let (core, path) = open_engine("sql49-expr-reject");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    insert(&core, &ctx, 1, "n, d, t, num", "1, 1.0, 'x', 1.00");
    let cases: [(&str, &str); 17] = [
        // 未知の修飾子は 42P01（カタログは照会しない）
        ("n = foo.n + 1", "42P01"),
        ("n = t.n * 2", "42P01"),
        // 未知の列
        ("n = zzz * 2", "42703"),
        ("n = EXCLUDED.zzz * 2", "42703"),
        ("n = EXCLUDED * 2", "42703"),
        // EXCLUDED から導出列・疑似列は参照できない
        ("n = EXCLUDED.id * 2", "42601"),
        ("n = EXCLUDED.tenant_id * 2", "42601"),
        ("n = EXCLUDED.visibility * 2", "42601"),
        // VECTOR 列の EXCLUDED 参照
        ("n = EXCLUDED.embedding * 2", "0A000"),
        // 型の食い違い
        ("n = upper(t)", "42804"),
        ("t = n * 2", "42804"),
        // NUMERIC 列への式・NUMERIC 列の参照
        ("num = d * 2", "0A000"),
        ("n = num * 2", "22000"),
        // 代入先の制限は従来どおり
        ("id = id * 2", "42601"),
        ("tenant_id = 1 * 2", "42601"),
        // 一般の式の中の `$n`・サブクエリ・UDF は対象外
        ("n = n * $1", "42601"),
        ("n = (SELECT 1)", "42601"),
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
    let (core, path) = open_engine("sql49-expr-tenant");
    let _g = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    insert(&core, &b, 1, "n", &i32::MAX.to_string());
    // tenant-a から見て id=1 は衝突しない（新規挿入）。22003 で他テナントの値を漏らさない。
    upsert(&core, &a, 1, "n", "1", "n = n * 2").expect("fresh insert for tenant-a");
    assert_eq!(cell(&core, &a, 1, "n"), int(1));
    assert_eq!(cell(&core, &b, 1, "n"), int(i64::from(i32::MAX)));
}

#[test]
fn explicit_transaction_sees_same_result() {
    let (core, path) = open_engine("sql49-expr-txn");
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
             ON CONFLICT (id) DO UPDATE SET n = n * 2 + EXCLUDED.n USING OPERATION_ID '{}'",
            op()
        );
        core.execute_sql_in_txn(&ctx, &mut session, &mut txn, &sql)
            .expect(&sql);
    }
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    // 1 → 1*2+1 = 3 → 3*2+1 = 7
    assert_eq!(cell(&core, &ctx, 1, "n"), int(7));
}
