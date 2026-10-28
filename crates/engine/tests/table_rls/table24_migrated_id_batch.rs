//! 移行済みの既存表で疑似列 `id` を省略した複数行 `INSERT`・`COPY FROM STDIN` の採番の結合テスト
//! （Issue #1700。親 #1696。単一行は #1699 の `table24_migrated_id_insert.rs`）。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-23・TABLE-24、`recovery.md` RECOVER-1・
//! RECOVER-10〜12、`rls.md` RLS-9・RLS-10。設計記録は `docs/design/migrated-id-identity.md`（D8・D9）。
//!
//! 検証の軸: 内容照合ハッシュは採番前の送信値で決まり、採番値は台帳に保存しない。
//!
//! - 1 文 = 1 台帳エントリで、行ごとに行順に採番する（`RETURNING id`・`currval` と一致）
//! - `USING OPERATION_ID` の再送は文全体が `23505` で、再実行せず SEQUENCE も進まない
//! - operation_id を省略した書き込みは台帳に残らず、同じ文の再実行は新しい値を採番する
//! - 採番値が既存行と衝突したら `23505` で全体 rollback
//! - COPY は autocommit・明示トランザクションの両方で採番し、`\N` を渡した `id` は `22000`

use engine::core::{CopyPlan, EngineCore};
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Visibility;

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session_legacy(&ctx(tenant), s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    let mut s = session();
    run(core, tenant, &mut s, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    let mut s = session();
    match run(core, tenant, &mut s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// `RETURNING id` の全行の `id`。
fn returned_ids(out: &SqlOutcome) -> Vec<u64> {
    match out {
        SqlOutcome::Returning(r) => r
            .result
            .rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(Cell::Integer(v)) => *v,
                other => panic!("id cell {other:?}"),
            })
            .collect(),
        other => panic!("expected RETURNING, got {other:?}"),
    }
}

/// 自テナントの `id` を昇順で返す。
fn ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    let mut v: Vec<u64> = core
        .execute_sql(&ctx(tenant), "SELECT id FROM items LIMIT 1000")
        .expect("select")
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

/// 従来方式の表 `items (n BIGINT)` に tenant-a の id 3・10 と tenant-b の id 5 を入れ、閉じて開き直す。
fn fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    {
        let core = EngineCore::open(&path).expect("open");
        let mut s = session();
        run(&core, "sys", &mut s, "CREATE TABLE items (n BIGINT)").expect("create");
        for (t, id) in [("tenant-a", 3), ("tenant-a", 10), ("tenant-b", 5)] {
            ok(
                &core,
                t,
                &format!("INSERT INTO items (id, n) VALUES ({id}, {id})"),
            );
        }
    }
    let core = EngineCore::open(&path).expect("reopen migrates");
    (guard, core)
}

/// 複数行の省略 INSERT は行順に自テナントの最大 id + 1 から連番を採番し、`RETURNING id` と一致する。
/// テナント間は独立。
#[test]
fn multi_row_omitted_id_is_assigned_in_row_order() {
    let (_g, core) = fixture("t24-batch-order");
    let mut s = session();
    let out = run(
        &core,
        "tenant-a",
        &mut s,
        "INSERT INTO items (n) VALUES (100), (101), (102) RETURNING id",
    )
    .expect("insert");
    assert_eq!(returned_ids(&out), vec![11, 12, 13]);
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 11, 12, 13]);
    let b = ok(
        &core,
        "tenant-b",
        "INSERT INTO items (n) VALUES (1), (2) RETURNING id",
    );
    assert_eq!(returned_ids(&b), vec![6, 7]);
    let c = ok(
        &core,
        "tenant-c",
        "INSERT INTO items (n) VALUES (1), (2) RETURNING id",
    );
    assert_eq!(returned_ids(&c), vec![1, 2]);
}

/// 受け入れ A1: operation_id 付きの再送は文全体が `23505` で、行も SEQUENCE も進まない。内容違いは `22023`。
#[test]
fn resend_with_operation_id_is_23505_and_does_not_consume_values() {
    let (_g, core) = fixture("t24-batch-resend");
    let sql = "INSERT INTO items (n) VALUES (1), (2) RETURNING id USING OPERATION_ID 'b-1'";
    assert_eq!(returned_ids(&ok(&core, "tenant-a", sql)), vec![11, 12]);
    assert_eq!(code(&core, "tenant-a", sql), "23505");
    assert_eq!(
        code(
            &core,
            "tenant-a",
            "INSERT INTO items (n) VALUES (1), (3) USING OPERATION_ID 'b-1'"
        ),
        "22023"
    );
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 11, 12]);
    let next = ok(
        &core,
        "tenant-a",
        "INSERT INTO items (n) VALUES (9) RETURNING id",
    );
    assert_eq!(returned_ids(&next), vec![13]);
}

/// 再起動を挟んでも台帳は永続し、再送は `23505` のまま値を消費しない。
#[test]
fn resend_after_restart_is_still_23505() {
    let path = unique_db_path("t24-batch-restart");
    let _g = CleanupGuard(path.clone());
    let sql = "INSERT INTO items (n) VALUES (1), (2) RETURNING id USING OPERATION_ID 'r-1'";
    {
        let core = EngineCore::open(&path).expect("open");
        let mut s = session();
        run(&core, "sys", &mut s, "CREATE TABLE items (n BIGINT)").expect("create");
    }
    {
        let core = EngineCore::open(&path).expect("reopen migrates");
        assert_eq!(returned_ids(&ok(&core, "tenant-a", sql)), vec![1, 2]);
    }
    let core = EngineCore::open(&path).expect("reopen");
    assert_eq!(code(&core, "tenant-a", sql), "23505");
    let next = ok(
        &core,
        "tenant-a",
        "INSERT INTO items (n) VALUES (3) RETURNING id",
    );
    assert_eq!(returned_ids(&next), vec![3]);
}

/// 受け入れ A2: operation_id 省略の書き込みは台帳に残らず、同じ文の再実行は新しい値を採番する。
#[test]
fn without_operation_id_nothing_is_recorded_and_rerun_assigns_new_values() {
    let (_g, core) = fixture("t24-batch-noop");
    let sql = "INSERT INTO items (n) VALUES (1), (2) RETURNING id";
    assert_eq!(returned_ids(&ok(&core, "tenant-a", sql)), vec![11, 12]);
    assert_eq!(returned_ids(&ok(&core, "tenant-a", sql)), vec![13, 14]);
}

/// 採番値が既存行と衝突したら `23505` で全体 rollback（行数不変）。自動で次の値へ飛ばない。
#[test]
fn collision_with_existing_row_rolls_back_the_whole_statement() {
    let (_g, core) = fixture("t24-batch-collide");
    // 次の採番値 11 の先（12）を明示 id で埋めておく。
    ok(
        &core,
        "tenant-a",
        "INSERT INTO items (id, n) VALUES (12, 0)",
    );
    assert_eq!(
        code(
            &core,
            "tenant-a",
            "INSERT INTO items (n) VALUES (1), (2), (3)"
        ),
        "23505"
    );
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 12]);
}

/// 明示 id の複数行は従来どおり。UPSERT の省略は `0A000` のまま。
#[test]
fn explicit_ids_keep_working_and_upsert_stays_unsupported() {
    let (_g, core) = fixture("t24-batch-explicit");
    ok(
        &core,
        "tenant-a",
        "INSERT INTO items (id, n) VALUES (20, 1), (21, 2)",
    );
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 20, 21]);
    assert_eq!(
        code(
            &core,
            "tenant-a",
            "INSERT INTO items (n) VALUES (1), (2) ON CONFLICT (id) DO NOTHING"
        ),
        "0A000"
    );
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 20, 21]);
}

/// D-g: operation_id 必須の構成では、採番より前に `23502` で止まり値を消費しない。
#[test]
fn required_operation_id_rejects_before_consuming_values() {
    let path = unique_db_path("t24-batch-required");
    let _g = CleanupGuard(path.clone());
    {
        let core = EngineCore::open(&path).expect("open");
        let mut s = session();
        run(&core, "sys", &mut s, "CREATE TABLE items (n BIGINT)").expect("create");
    }
    let core = EngineCore::open(&path)
        .expect("reopen")
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    assert_eq!(
        code(&core, "tenant-a", "INSERT INTO items (n) VALUES (1), (2)"),
        "23502"
    );
    let out = ok(
        &core,
        "tenant-a",
        "INSERT INTO items (n) VALUES (1), (2) RETURNING id USING OPERATION_ID 'q-1'",
    );
    assert_eq!(returned_ids(&out), vec![1, 2]);
}

/// COPY の駆動ヘルパー（wire-server の CopyIn 完了処理と同じ `commit_copy_in_txn_in_session` を使う）。
fn copy_in<'a>(
    core: &'a EngineCore,
    tenant: &str,
    sql: &str,
    data: &[u8],
    s: &mut SessionState,
    txn: &mut engine::sql::transaction::SessionTransaction<'a>,
) -> Result<u64, SqlSurfaceError> {
    let policy = ctx(tenant);
    let plan = core.begin_copy(&policy, s, sql)?;
    let CopyPlan::From(mut cs) = plan else {
        panic!("expected COPY FROM STDIN plan");
    };
    cs.feed(data)?;
    let batch = cs.finish()?;
    core.commit_copy_in_txn_in_session(&policy, s, txn, batch)
        .map(|o| o.rows_affected)
}

/// COPY（autocommit）: `id` を列リストから省略すると行順に採番し、再送は `23505`。`id` 列を含めて
/// `\N` を渡す形は `22000`。
#[test]
fn copy_omitting_id_assigns_and_resend_is_23505() {
    let (_g, core) = fixture("t24-batch-copy");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    let sql = "COPY items (n) FROM STDIN USING OPERATION_ID 'c-1'";
    let n = copy_in(&core, "tenant-a", sql, b"1\n2\n3\n", &mut s, &mut txn).expect("copy");
    assert_eq!(n, 3);
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 11, 12, 13]);
    let again = copy_in(&core, "tenant-a", sql, b"1\n2\n3\n", &mut s, &mut txn);
    assert_eq!(again.expect_err("resend").wire_code(), "23505");
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 11, 12, 13]);
    // 値を消費していなければ、続く INSERT は 14。
    let next = ok(
        &core,
        "tenant-a",
        "INSERT INTO items (n) VALUES (9) RETURNING id",
    );
    assert_eq!(returned_ids(&next), vec![14]);
    let null_id = copy_in(
        &core,
        "tenant-a",
        "COPY items (id, n) FROM STDIN",
        b"\\N\t1\n",
        &mut s,
        &mut txn,
    );
    assert_eq!(null_id.expect_err("null id").wire_code(), "22000");
    // 一行だけの COPY も採番する。
    let one = copy_in(
        &core,
        "tenant-b",
        "COPY items (n) FROM STDIN",
        b"7\n",
        &mut s,
        &mut txn,
    );
    assert_eq!(one.expect("copy one"), 1);
    assert_eq!(ids(&core, "tenant-b"), vec![5, 6]);
}

/// COPY（明示トランザクション）: 採番でき、ROLLBACK した分は欠番になる（D7）。
#[test]
fn copy_omitting_id_in_explicit_transaction() {
    let (_g, core) = fixture("t24-batch-copy-txn");
    let mut s = session();
    let c = ctx("tenant-a");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let n = copy_in(
        &core,
        "tenant-a",
        "COPY items (n) FROM STDIN",
        b"1\n2\n",
        &mut s,
        &mut txn,
    )
    .expect("copy in txn");
    assert_eq!(n, 2);
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10]);
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    copy_in(
        &core,
        "tenant-a",
        "COPY items (n) FROM STDIN",
        b"5\n6\n",
        &mut s,
        &mut txn,
    )
    .expect("copy in txn");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "COMMIT")
        .expect("commit");
    // ROLLBACK した 11・12 は欠番。
    assert_eq!(ids(&core, "tenant-a"), vec![3, 10, 13, 14]);
}

/// 従来方式のままの表（開き直すまで）の COPY は `id` 列が必須の `22000` を維持する。
#[test]
fn copy_to_legacy_table_still_requires_id() {
    let (_g, core) = fixture("t24-batch-copy-legacy");
    let mut s = session();
    run(&core, "sys", &mut s, "CREATE TABLE fresh (n BIGINT)").expect("create");
    let mut txn = core.new_session_transaction();
    let r = copy_in(
        &core,
        "tenant-a",
        "COPY fresh (n) FROM STDIN",
        b"1\n",
        &mut s,
        &mut txn,
    );
    assert_eq!(r.expect_err("must fail").wire_code(), "22000");
}
