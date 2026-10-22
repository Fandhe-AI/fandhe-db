//! `INSERT ... ON CONFLICT ... DO UPDATE SET ... WHERE <条件>`（SQL-49 (3)・RECOVER-11・RLS-9、
//! Issue #1740）の結合テスト。
//!
//! ポインタ: `docs/spec/04-behavior/` の SQL-49・RECOVER-11・RLS-9。`EngineCore::
//! execute_sql_in_session`（parser の `WHERE` 解析 → `sql::upsert_where` の束縛 →
//! `tenant::upsert_typed_rows_unchecked` の `DoUpdate` 腕での行ごとの評価）を production 経路として
//! 検証する。期待値は PostgreSQL の `ON CONFLICT DO UPDATE ... WHERE`（条件が偽または NULL の
//! 行は更新せず、件数・`RETURNING` にも数えない）に合わせてある。
//! 内容照合ハッシュへの `WHERE` の反映（再送の `22023`／`23505`）も本テストで固定する。
//! ビュー経由・`$n` は本テストの対象外。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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
    format!("op-where-{}", OP_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("nn", ColumnType::Integer, false),
            ColumnDef::new("m", ColumnType::Integer, true),
            ColumnDef::new("t", ColumnType::Text, true),
            ColumnDef::new("flag", ColumnType::Boolean, true),
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

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Insert(o) => o.rows_affected,
        SqlOutcome::Returning(o) => o.rows_affected,
        other => panic!("expected Insert/Returning outcome, got {other:?}"),
    }
}

fn seed(core: &EngineCore, ctx: &PolicyContext, id: u64, n: &str, m: &str, t: &str) {
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n, m, t) \
         VALUES ({id}, '[0.1,0.2]', 0, {n}, {m}, {t}) USING OPERATION_ID '{}'",
        op()
    );
    run(core, ctx, &sql).expect(&sql);
}

/// `VALUES (id, n, m, t)` の 1 行を `ON CONFLICT (id) DO UPDATE SET <set> WHERE <wh>` で送る。
fn upsert_where(
    core: &EngineCore,
    ctx: &PolicyContext,
    row: (u64, &str, &str, &str),
    set: &str,
    wh: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let (id, n, m, t) = row;
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n, m, t) \
         VALUES ({id}, '[0.1,0.2]', 0, {n}, {m}, {t}) \
         ON CONFLICT (id) DO UPDATE SET {set} WHERE {wh} USING OPERATION_ID '{}'",
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
fn true_false_and_null_conditions_control_the_update() {
    let (core, path) = open_engine("sql49-where-truth");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "10", "NULL", "'x'");

    // 真: 10 < 20 なので更新され、件数に数える。
    let out = upsert_where(
        &core,
        &ctx,
        (1, "20", "NULL", "'y'"),
        "n = EXCLUDED.n",
        "n < EXCLUDED.n",
    )
    .expect("true");
    assert_eq!(affected(out), 1);
    assert_eq!(cell(&core, &ctx, 1, "n"), int(20));

    // 偽: 20 > 5。行は不変で件数は 0。
    let out = upsert_where(
        &core,
        &ctx,
        (1, "5", "NULL", "'z'"),
        "n = EXCLUDED.n, t = EXCLUDED.t",
        "n < EXCLUDED.n",
    )
    .expect("false");
    assert_eq!(affected(out), 0);
    assert_eq!(cell(&core, &ctx, 1, "n"), int(20));
    assert_eq!(cell(&core, &ctx, 1, "t"), Cell::Text("x".to_string()));

    // NULL（既存行の m が NULL）: 偽と同じ扱いで更新しない。
    let out = upsert_where(
        &core,
        &ctx,
        (1, "99", "7", "'q'"),
        "n = EXCLUDED.n",
        "m < EXCLUDED.m",
    )
    .expect("null");
    assert_eq!(affected(out), 0);
    assert_eq!(cell(&core, &ctx, 1, "n"), int(20));

    // NOT (NULL) も NULL のまま非該当（NOT の押し下げでも三値論理が保たれる）。
    let out = upsert_where(
        &core,
        &ctx,
        (1, "99", "7", "'q'"),
        "n = EXCLUDED.n",
        "NOT (m < EXCLUDED.m)",
    )
    .expect("not null");
    assert_eq!(affected(out), 0);
    assert_eq!(cell(&core, &ctx, 1, "n"), int(20));

    // NULL を IS NULL で真にできる。
    let out = upsert_where(
        &core,
        &ctx,
        (1, "99", "7", "'q'"),
        "n = EXCLUDED.n",
        "m IS NULL",
    )
    .expect("is null");
    assert_eq!(affected(out), 1);
    assert_eq!(cell(&core, &ctx, 1, "n"), int(99));
    assert_eq!(row_count(&core, &ctx), 1);
}

#[test]
fn reference_forms_and_predicate_shapes() {
    let (core, path) = open_engine("sql49-where-forms");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "10", "3", "'x'");
    let cases: [(&str, u64); 11] = [
        ("n = 10", 1),
        ("counters.n = 10", 1),
        ("counters.n < excluded.n", 1),
        ("EXCLUDED.n > counters.n", 1),
        ("n + 1 = 11", 1),
        ("n IN (10, 11) AND m BETWEEN 1 AND 5", 1),
        ("n = 1 OR m = 3", 1),
        ("(n = 1 OR m = 3) AND t = 'x'", 1),
        ("t LIKE 'x%'", 1),
        ("n = 11", 0),
        ("counters.n > excluded.n", 0),
    ];
    for (wh, expected) in cases {
        let out = upsert_where(&core, &ctx, (1, "50", "3", "'x'"), "m = EXCLUDED.m", wh).expect(wh);
        assert_eq!(affected(out), expected, "{wh}");
    }
    // 各行の SET は no-op（`m = m` 相当）なので値は変わらない。
    assert_eq!(cell(&core, &ctx, 1, "n"), int(10));
}

#[test]
fn boolean_column_conditions() {
    let (core, path) = open_engine("sql49-where-bool");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n, flag) VALUES (1, '[0.1,0.2]', 0, 1, true) \
         USING OPERATION_ID '{}'",
        op()
    );
    run(&core, &ctx, &sql).expect("seed");
    let mk = |wh: &str| {
        format!(
            "INSERT INTO {TABLE} (id, embedding, nn, n, flag) VALUES (1, '[0.1,0.2]', 0, 2, false) \
             ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE {wh} USING OPERATION_ID '{}'",
            op()
        )
    };
    assert_eq!(affected(run(&core, &ctx, &mk("NOT flag")).expect("not")), 0);
    assert_eq!(cell(&core, &ctx, 1, "n"), int(1));
    assert_eq!(affected(run(&core, &ctx, &mk("flag")).expect("flag")), 1);
    assert_eq!(cell(&core, &ctx, 1, "n"), int(2));
}

#[test]
fn multi_row_mix_counts_only_inserted_and_updated_rows() {
    let (core, path) = open_engine("sql49-where-multi");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "1", "NULL", "NULL");
    seed(&core, &ctx, 2, "100", "NULL", "NULL");
    // 1 は条件が真（1 < 50）、2 は偽（100 < 60 でない）、3 は新規挿入。
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES \
         (1, '[0.1,0.2]', 0, 50), (2, '[0.1,0.2]', 0, 60), (3, '[0.1,0.2]', 0, 70) \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE n < EXCLUDED.n \
         RETURNING id, n USING OPERATION_ID '{}'",
        op()
    );
    let SqlOutcome::Returning(out) = run(&core, &ctx, &sql).expect(&sql) else {
        panic!("expected Returning outcome");
    };
    assert_eq!(out.rows_affected, 2);
    let mut got: Vec<(Cell, Cell)> = out
        .result
        .rows
        .iter()
        .map(|r| (r.cells[0].clone(), r.cells[1].clone()))
        .collect();
    got.sort_by_key(|(id, _)| format!("{id:?}"));
    assert_eq!(got.len(), 2, "{got:?}");
    let ids: Vec<String> = got.iter().map(|(id, _)| format!("{id:?}")).collect();
    assert!(ids.iter().any(|s| s.contains('1')), "{ids:?}");
    assert!(ids.iter().any(|s| s.contains('3')), "{ids:?}");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(50));
    assert_eq!(cell(&core, &ctx, 2, "n"), int(100));
    assert_eq!(cell(&core, &ctx, 3, "n"), int(70));
}

#[test]
fn returning_shape_is_unchanged_and_skipped_rows_are_not_returned() {
    let (core, path) = open_engine("sql49-where-returning");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "10", "NULL", "'x'");
    let with_where = |wh: &str| {
        format!(
            "INSERT INTO {TABLE} (id, embedding, nn, n, t) VALUES (1, '[0.1,0.2]', 0, 20, 'w') \
             ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE {wh} \
             RETURNING * USING OPERATION_ID '{}'",
            op()
        )
    };
    let without_where = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n, t) VALUES (1, '[0.1,0.2]', 0, 21, 'w') \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n RETURNING * USING OPERATION_ID '{}'",
        op()
    );
    let SqlOutcome::Returning(skipped) = run(&core, &ctx, &with_where("n > 100")).expect("skip")
    else {
        panic!("expected Returning outcome");
    };
    assert_eq!(skipped.rows_affected, 0);
    assert!(skipped.result.rows.is_empty());
    let SqlOutcome::Returning(applied) = run(&core, &ctx, &with_where("n < 100")).expect("apply")
    else {
        panic!("expected Returning outcome");
    };
    assert_eq!(applied.rows_affected, 1);
    assert_eq!(applied.result.rows.len(), 1);
    let SqlOutcome::Returning(plain) = run(&core, &ctx, &without_where).expect("plain") else {
        panic!("expected Returning outcome");
    };
    // 列構成（名前・型・順序）は WHERE の有無で変わらない。
    assert_eq!(
        format!("{:?}", skipped.result.columns),
        format!("{:?}", plain.result.columns)
    );
    assert_eq!(
        format!("{:?}", applied.result.columns),
        format!("{:?}", plain.result.columns)
    );
}

#[test]
fn skipped_rows_do_not_evaluate_the_set_expression() {
    let (core, path) = open_engine("sql49-where-order");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, &i32::MAX.to_string(), "0", "NULL");
    // 偽の行では加算の範囲外（22003）が起きない。
    let out = upsert_where(&core, &ctx, (1, "1", "0", "NULL"), "n = n + 1", "m = 1").expect("skip");
    assert_eq!(affected(out), 0);
    assert_eq!(cell(&core, &ctx, 1, "n"), int(i64::from(i32::MAX)));
    // 真の行では評価エラーが文全体を abort し、行は変わらない。
    let err = upsert_where(&core, &ctx, (1, "1", "0", "NULL"), "n = n + 1", "m = 0")
        .expect_err("overflow")
        .wire_code();
    assert_eq!(err, "22003");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(i64::from(i32::MAX)));
}

#[test]
fn condition_evaluation_error_aborts_the_whole_statement() {
    let (core, path) = open_engine("sql49-where-eval-err");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "10", "0", "NULL");
    seed(&core, &ctx, 2, "10", "5", "NULL");
    // 2 行目で 0 除算（5 - 5 = 0）。1 行目の更新も残さない。
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, nn, n, m) VALUES \
         (1, '[0.1,0.2]', 0, 11, 1), (2, '[0.1,0.2]', 0, 12, 5) \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE 10 / (m - EXCLUDED.m + 1) > 0 \
         USING OPERATION_ID '{}'",
        op()
    );
    let got = code(&core, &ctx, &sql);
    assert!(got == "22012", "unexpected wire code {got}");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(10));
    assert_eq!(cell(&core, &ctx, 2, "n"), int(10));
}

#[test]
fn unsupported_shapes_are_rejected_fail_closed() {
    let (core, path) = open_engine("sql49-where-reject");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "1", "1", "'x'");
    let cases: [(&str, &str); 12] = [
        ("n IN (SELECT n FROM counters)", "42601"),
        ("EXISTS (SELECT n FROM counters)", "42601"),
        ("visible()", "42601"),
        ("EXCLUDED.id = 1", "42601"),
        ("EXCLUDED.tenant_id = 'a'", "42601"),
        ("tenant_id = 'a'", "42601"),
        ("other.n = 1", "42P01"),
        ("zzz = 1", "42703"),
        ("EXCLUDED.zzz = 1", "42703"),
        ("embedding IS NULL", "0A000"),
        ("EXCLUDED.embedding IS NULL", "0A000"),
        ("my_udf(n) > 1", "42601"),
    ];
    for (wh, expected) in cases {
        let got = upsert_where(&core, &ctx, (1, "1", "1", "'x'"), "n = EXCLUDED.n", wh)
            .expect_err(wh)
            .wire_code();
        assert_eq!(got, expected, "{wh}");
    }
    assert_eq!(cell(&core, &ctx, 1, "n"), int(1));
}

#[test]
fn do_nothing_with_where_and_trailing_garbage_stay_syntax_errors() {
    let (core, path) = open_engine("sql49-where-syntax");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "1", "1", "'x'");
    let head = format!("INSERT INTO {TABLE} (id, embedding, nn, n) VALUES (1, '[0.1,0.2]', 0, 2)");
    let cases = [
        format!("{head} ON CONFLICT (id) DO NOTHING WHERE n = 1 USING OPERATION_ID 'o-1'"),
        format!("{head} ON CONFLICT DO NOTHING WHERE n = 1 USING OPERATION_ID 'o-2'"),
        format!(
            "{head} ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE USING OPERATION_ID 'o-3'"
        ),
        format!(
            "{head} ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE n = 1 LIMIT 1 \
             USING OPERATION_ID 'o-4'"
        ),
        format!(
            "{head} ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE n = 1 WHERE n = 1 \
             USING OPERATION_ID 'o-5'"
        ),
        format!("{head} ON CONFLICT (id) WHERE n = 1 DO NOTHING USING OPERATION_ID 'o-6'"),
    ];
    for sql in cases {
        assert_eq!(code(&core, &ctx, &sql), "42601", "{sql}");
    }
    assert_eq!(cell(&core, &ctx, 1, "n"), int(1));
}

#[test]
fn invisible_own_row_is_rejected_regardless_of_the_condition() {
    let (core, path) = open_engine("sql49-where-invisible");
    let _g = CleanupGuard(path);
    let full = ctx_for("tenant-a");
    seed(&core, &full, 1, "1", "1", "'x'");
    // 同テナントでも Public しか見えない文脈からは、Private 行の衝突は不可視（42501）。
    // 条件が真でも偽でも結果は同じで、値依存のエラーも出ない（存在オラクルにしない）。
    let public_only =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("ctx");
    for wh in ["n = 1", "n = 2", "10 / (n - 1) > 0"] {
        let got = upsert_where(
            &core,
            &public_only,
            (1, "9", "1", "'x'"),
            "n = EXCLUDED.n",
            wh,
        )
        .expect_err(wh)
        .wire_code();
        assert_eq!(got, "42501", "{wh}");
    }
    assert_eq!(cell(&core, &full, 1, "n"), int(1));
}

#[test]
fn other_tenant_row_is_a_fresh_insert_and_never_evaluated() {
    let (core, path) = open_engine("sql49-where-tenant");
    let _g = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    seed(&core, &b, 1, "7", "0", "'b'");
    // tenant-a から見て id=1 は衝突しない。条件が（もし評価されれば）0 除算になる形でも評価されない。
    let out = upsert_where(
        &core,
        &a,
        (1, "1", "1", "'a'"),
        "n = EXCLUDED.n",
        "10 / (m - m) > 0",
    )
    .expect("fresh insert");
    assert_eq!(affected(out), 1);
    assert_eq!(cell(&core, &a, 1, "n"), int(1));
    assert_eq!(cell(&core, &b, 1, "n"), int(7));
}

#[test]
fn explicit_transaction_and_ledger_replay() {
    let (core, path) = open_engine("sql49-where-txn");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "1", "1", "'x'");
    let mut txn = core.new_session_transaction();
    let mut session = SessionState::default();
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let mk = |wh: &str| {
        format!(
            "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES (1, '[0.1,0.2]', 0, 5) \
             ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE {wh} USING OPERATION_ID '{}'",
            op()
        )
    };
    let skipped = core
        .execute_sql_in_txn(&ctx, &mut session, &mut txn, &mk("n > 100"))
        .expect("skip in txn");
    assert_eq!(affected(skipped), 0);
    let applied = core
        .execute_sql_in_txn(&ctx, &mut session, &mut txn, &mk("n < 100"))
        .expect("apply in txn");
    assert_eq!(affected(applied), 1);
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(5));

    // 同じ operation_id・同じ文の再送は冪等に拒否される（二重適用しない）。
    let replay = "INSERT INTO counters (id, embedding, nn, n) VALUES (1, '[0.1,0.2]', 0, 9) \
                  ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE n < 100 \
                  USING OPERATION_ID 'op-where-replay'";
    run(&core, &ctx, replay).expect("first");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(9));
    assert_eq!(code(&core, &ctx, replay), "23505");
    assert_eq!(cell(&core, &ctx, 1, "n"), int(9));
}

#[test]
fn unique_target_conditions_see_the_conflicting_row() {
    let path = unique_db_path("sql49-where-unique");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let schema = TableSchema::new(
        "kv",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("k", ColumnType::Text, false),
            ColumnDef::new("v", ColumnType::Integer, true),
        ],
    )
    .with_primary_key(vec!["k".to_string()]);
    storage.create_table(&schema).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    let ctx = ctx_for("tenant-a");
    let first = format!(
        "INSERT INTO kv (id, embedding, k, v) VALUES (1, '[0.1,0.2]', 'a', 5) \
         USING OPERATION_ID '{}'",
        op()
    );
    run(&core, &ctx, &first).expect("seed");
    for (v, expected) in [(3, 0u64), (8, 1u64)] {
        let sql = format!(
            "INSERT INTO kv (id, embedding, k, v) VALUES (2, '[0.1,0.2]', 'a', {v}) \
             ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v WHERE kv.v < EXCLUDED.v \
             USING OPERATION_ID '{}'",
            op()
        );
        assert_eq!(affected(run(&core, &ctx, &sql).expect(&sql)), expected);
    }
    let SqlOutcome::Query(result) = run(&core, &ctx, "SELECT v FROM kv LIMIT 10").expect("select")
    else {
        panic!("expected Query outcome");
    };
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], int(8));
}

/// 同じ `operation_id` で WHERE の有無・内容だけが異なる再送は、内容照合ハッシュ不一致の
/// `22023` で拒否される（RECOVER-11。`23505` の冪等再送扱いにしない）。
#[test]
fn differing_where_under_same_operation_id_is_a_content_mismatch() {
    let (core, path) = open_engine("sql49-where-hash");
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx, 1, "1", "1", "'x'");
    let mk = |wh: Option<&str>| {
        let w = wh.map(|c| format!(" WHERE {c}")).unwrap_or_default();
        format!(
            "INSERT INTO {TABLE} (id, embedding, nn, n) VALUES (1, '[0.1,0.2]', 0, 5) \
             ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n{w} USING OPERATION_ID 'op-where-hash'"
        )
    };
    run(&core, &ctx, &mk(Some("n < 100"))).expect("first");
    assert_eq!(code(&core, &ctx, &mk(Some("n < 200"))), "22023");
    assert_eq!(code(&core, &ctx, &mk(None)), "22023");
    // 同一内容の再送は従来どおり冪等拒否。
    assert_eq!(code(&core, &ctx, &mk(Some("n < 100"))), "23505");
}

/// `using`／`returning` という名前の列（引用付き含む）は条件区間を閉じない。
#[test]
fn using_and_returning_named_columns_do_not_end_the_condition() {
    let path = unique_db_path("sql49-where-kwcols");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let schema = TableSchema::new(
        "kw",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("using", ColumnType::Integer, true),
            ColumnDef::new("returning", ColumnType::Integer, true),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    let ctx = ctx_for("tenant-a");
    run(
        &core,
        &ctx,
        "INSERT INTO kw (id, embedding, \"using\", \"returning\") VALUES (1, '[0.1,0.2]', 1, 1) \
         USING OPERATION_ID 'op-kw-seed'",
    )
    .expect("seed");
    for (i, wh) in [
        "\"using\" = 1",
        "using = 1",
        "\"returning\" = 1",
        "returning = 1",
        "\"using\" = 1 AND \"returning\" = 1",
    ]
    .iter()
    .enumerate()
    {
        let sql = format!(
            "INSERT INTO kw (id, embedding, \"using\") VALUES (1, '[0.1,0.2]', 7) \
             ON CONFLICT (id) DO UPDATE SET \"using\" = 7 WHERE {wh} \
             RETURNING id USING OPERATION_ID 'op-kw-{i}'"
        );
        let out = run(&core, &ctx, &sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        assert_eq!(affected(out), 1, "{sql}");
        // 次の反復も WHERE を真にするため元に戻す。
        let reset = format!(
            "INSERT INTO kw (id, embedding, \"using\") VALUES (1, '[0.1,0.2]', 1) \
             ON CONFLICT (id) DO UPDATE SET \"using\" = 1 WHERE id = 1 \
             USING OPERATION_ID 'op-kw-reset-{i}'"
        );
        run(&core, &ctx, &reset).expect("reset");
    }
}
