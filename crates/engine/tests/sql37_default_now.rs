//! 列 `DEFAULT now()` 系と台帳の再送一貫性の結合テスト（Issue #1587、対象ビヘイビア: SQL-37・
//! RECOVER-10・RECOVER-11・RECOVER-12・TABLE-16・TASK-248。ポインタ:
//! `docs/spec/04-behavior/sql-surface.md`・`docs/design/now-evaluated-value-ledger.md` D1・D2・D9）。
//!
//! `EngineCore::execute_sql_in_session`／`execute_sql_in_txn`（production 経路そのもの）で、
//! 時刻 DEFAULT の受理・カタログ往復・INSERT／UPSERT／RETURNING／COPY の省略列補完・
//! 同一 `operation_id` の再送（`23505`。評価時刻が違っても行は書き換わらない）・
//! クラッシュ復旧（`Storage` を閉じて再オープン）後の再送・内容不一致（`22023`）・
//! テナント独立・未対応経路の fail-closed 拒否を検証する。
//! 時刻の絶対値には依存せず、同値・型・エラーコードだけを見る。

use std::time::Duration;

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

/// 時刻 DEFAULT 列を持つ表を作る。`at`（TIMESTAMP）は `now()`、`at2` は `CURRENT_TIMESTAMP(3)`、
/// `d` は `CURRENT_DATE`。
fn create_events(core: &EngineCore) {
    let mut session = ddl_session();
    core.execute_sql_in_session(
        &ctx_for("tenant-a"),
        &mut session,
        "CREATE TABLE events (embedding VECTOR(2), lang TEXT, \
         at TIMESTAMP DEFAULT now(), at2 TIMESTAMP DEFAULT CURRENT_TIMESTAMP(3), \
         d DATE DEFAULT CURRENT_DATE)",
    )
    .expect("create table with time defaults");
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx_for(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    exec(core, tenant, sql).expect_err(sql)
}

fn select_all(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match exec(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn ts(cell: &Cell) -> i64 {
    match cell {
        Cell::Timestamp(v) => *v,
        other => panic!("expected Cell::Timestamp, got {other:?}"),
    }
}

fn pause() {
    std::thread::sleep(Duration::from_millis(15));
}

const INSERT_ONE: &str = "INSERT INTO events (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
                          USING OPERATION_ID 'op-1'";

#[test]
fn create_table_accepts_time_defaults_and_roundtrips_across_reopen() {
    let path = unique_db_path("sql37-default-now-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let core = open_core(&path);
        create_events(&core);
    }
    // 再オープン後もカタログから復元され、省略列へ時刻が入る。
    let core = open_core(&path);
    exec(&core, "tenant-a", INSERT_ONE).expect("insert after reopen");
    let rows = select_all(&core, "tenant-a", "SELECT at, at2, d FROM events LIMIT 10");
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Cell::Timestamp(_)));
    assert!(matches!(rows[0][1], Cell::Timestamp(_)));
    assert!(matches!(rows[0][2], Cell::Date(_)));
}

#[test]
fn unsupported_or_mismatched_time_defaults_are_rejected() {
    let path = unique_db_path("sql37-default-now-reject");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut session = ddl_session();
    let mut ddl = |sql: &str| -> String {
        core.execute_sql_in_session(&ctx_for("tenant-a"), &mut session, sql)
            .expect_err(sql)
            .wire_code()
            .to_string()
    };
    // `CURRENT_TIME`（timetz 未実装）は引き続き拒否する。
    assert_eq!(
        ddl("CREATE TABLE t1 (embedding VECTOR(2), n TIME DEFAULT CURRENT_TIME)"),
        "42601"
    );
    // LOCALTIME は TIME 列にだけ付けられる（TIMESTAMP 列は型不一致）。
    assert_eq!(
        ddl("CREATE TABLE t2 (embedding VECTOR(2), n TIMESTAMP DEFAULT LOCALTIME)"),
        "42601"
    );
    // 複合式・CHECK 内は 42601。
    assert_eq!(
        ddl("CREATE TABLE t3 (embedding VECTOR(2), n TIMESTAMP DEFAULT now() + 1)"),
        "42601"
    );
    assert_eq!(
        ddl("CREATE TABLE t4 (embedding VECTOR(2), n TIMESTAMP, CHECK (n < now()))"),
        "42601"
    );
    // 型不一致（TEXT 列に now()、DATE 列に now()、TIMESTAMP 列に CURRENT_DATE）。
    assert_eq!(
        ddl("CREATE TABLE t5 (embedding VECTOR(2), n TEXT DEFAULT now())"),
        "42601"
    );
    assert_eq!(
        ddl("CREATE TABLE t6 (embedding VECTOR(2), n DATE DEFAULT now())"),
        "42601"
    );
    assert_eq!(
        ddl("CREATE TABLE t7 (embedding VECTOR(2), n TIMESTAMP DEFAULT CURRENT_DATE)"),
        "42601"
    );
    // 精度範囲外は 22023。
    assert_eq!(
        ddl("CREATE TABLE t8 (embedding VECTOR(2), n TIMESTAMP DEFAULT CURRENT_TIMESTAMP(7))"),
        "22023"
    );
    // VECTOR 列に時刻 DEFAULT は付けられない。
    assert_eq!(
        ddl("CREATE TABLE t9 (embedding VECTOR(2) DEFAULT now())"),
        "42601"
    );
}

#[test]
fn add_column_and_set_default_fk_with_time_default_are_rejected() {
    let path = unique_db_path("sql37-default-now-add-column");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    create_events(&core);
    let mut session = ddl_session();
    let ctx = ctx_for("tenant-a");
    let mut ddl = |sql: &str| -> Result<(), String> {
        core.execute_sql_in_session(&ctx, &mut session, sql)
            .map(|_| ())
            .map_err(|e| e.wire_code().to_string())
    };
    // ADD COLUMN ... DEFAULT now() は既存行の補完値を固定できないため拒否（設計 D10）。
    assert_eq!(
        ddl("ALTER TABLE events ADD COLUMN added TIMESTAMP DEFAULT now()"),
        Err("0A000".to_string())
    );
    // 時刻 DEFAULT 列を `ON DELETE SET DEFAULT` の対象にする FK の宣言は 0A000（設計 D9）。
    ddl("CREATE TABLE parents (embedding VECTOR(2), k TIMESTAMP, UNIQUE (k))")
        .expect("parent table");
    assert_eq!(
        ddl("CREATE TABLE child (embedding VECTOR(2), \
             ts TIMESTAMP DEFAULT now() REFERENCES parents (k) ON DELETE SET DEFAULT)"),
        Err("0A000".to_string())
    );
    assert_eq!(
        ddl(
            "ALTER TABLE events ADD FOREIGN KEY (at) REFERENCES parents (k) \
             ON UPDATE SET DEFAULT"
        ),
        Err("0A000".to_string())
    );
}

#[test]
fn insert_applies_default_to_omitted_columns_only() {
    let path = unique_db_path("sql37-default-now-insert");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    create_events(&core);

    // 複数行 VALUES は全行が同じ時刻（文の開始時刻）になる。
    exec(
        &core,
        "tenant-a",
        "INSERT INTO events (id, embedding, lang) VALUES \
         (1, '[0.1,0.2]', 'a'), (2, '[0.1,0.2]', 'b'), (3, '[0.1,0.2]', 'c') \
         USING OPERATION_ID 'op-multi'",
    )
    .expect("multi-row insert");
    let rows = select_all(&core, "tenant-a", "SELECT at, at2, d FROM events LIMIT 10");
    assert_eq!(rows.len(), 3);
    let first = ts(&rows[0][0]);
    assert!(
        rows.iter().all(|r| ts(&r[0]) == first),
        "same at in a statement"
    );
    // `CURRENT_TIMESTAMP(3)` はミリ秒へ丸められる。
    assert!(rows.iter().all(|r| ts(&r[1]) % 1000 == 0));
    assert!(rows.iter().all(|r| matches!(r[2], Cell::Date(_))));

    // 明示値・明示 NULL には DEFAULT を適用しない。
    exec(
        &core,
        "tenant-a",
        "INSERT INTO events (id, embedding, lang, at, at2) VALUES \
         (10, '[0.1,0.2]', 'x', '2000-01-01 00:00:00', NULL) USING OPERATION_ID 'op-explicit'",
    )
    .expect("explicit values");
    let rows = select_all(
        &core,
        "tenant-a",
        "SELECT at, at2 FROM events WHERE id = 10 LIMIT 10",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(ts(&rows[0][0]), 946_684_800_000_000);
    assert!(matches!(rows[0][1], Cell::Null));
}

#[test]
fn explicit_transaction_uses_transaction_start_time() {
    let path = unique_db_path("sql37-default-now-txn");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    create_events(&core);
    exec(
        &core,
        "tenant-a",
        "INSERT INTO events (id, embedding, lang, at) VALUES \
         (99, '[0.1,0.2]', 'seed', '2000-01-01 00:00:00') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed");

    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    pause();
    let begin_now = match core
        .execute_sql_in_txn(
            &ctx,
            &mut session,
            &mut txn,
            "SELECT now() FROM events WHERE id = 99 LIMIT 10",
        )
        .expect("select now()")
    {
        SqlOutcome::Query(r) => ts(&r.rows[0].cells[0]),
        other => panic!("expected Query, got {other:?}"),
    };
    pause();
    for (id, op) in [(1, "op-t1"), (2, "op-t2")] {
        core.execute_sql_in_txn(
            &ctx,
            &mut session,
            &mut txn,
            &format!(
                "INSERT INTO events (id, embedding, lang) VALUES ({id}, '[0.1,0.2]', 'x') \
                 USING OPERATION_ID '{op}'"
            ),
        )
        .expect("insert in txn");
        pause();
    }
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    let rows = select_all(
        &core,
        "tenant-a",
        "SELECT at FROM events WHERE id <= 2 LIMIT 10",
    );
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|r| ts(&r[0]) == begin_now),
        "DEFAULT now() is the transaction start time for every statement"
    );
}

#[test]
fn resend_with_same_operation_id_is_duplicate_and_keeps_the_first_value() {
    let path = unique_db_path("sql37-default-now-resend");
    let _g = CleanupGuard(path.clone());
    let first_at;
    {
        let core = open_core(&path);
        create_events(&core);
        exec(&core, "tenant-a", INSERT_ONE).expect("first send");
        first_at = ts(&select_all(&core, "tenant-a", "SELECT at FROM events LIMIT 10")[0][0]);
        pause();
        // 同じ文の再送は評価時刻が変わっていても 23505（行は書き換わらない）。
        assert_eq!(code(&core, "tenant-a", INSERT_ONE), "23505");
        let rows = select_all(&core, "tenant-a", "SELECT at FROM events LIMIT 10");
        assert_eq!(rows.len(), 1);
        assert_eq!(ts(&rows[0][0]), first_at);
    }
    // クラッシュ復旧（Storage を閉じて再オープン）後の再送も 23505・値は初回のまま。
    let core = open_core(&path);
    pause();
    assert_eq!(code(&core, "tenant-a", INSERT_ONE), "23505");
    let rows = select_all(&core, "tenant-a", "SELECT at FROM events LIMIT 10");
    assert_eq!(rows.len(), 1);
    assert_eq!(ts(&rows[0][0]), first_at);

    // 同じ operation_id で列を明示した別内容の文は 22023（誤用）。
    assert_eq!(
        code(
            &core,
            "tenant-a",
            "INSERT INTO events (id, embedding, lang, at) VALUES \
             (1, '[0.1,0.2]', 'ja', '2000-01-01 00:00:00') USING OPERATION_ID 'op-1'"
        ),
        "22023"
    );
    // 他テナントの同じ operation_id は独立に成功する。
    exec(&core, "tenant-b", INSERT_ONE).expect("tenant-b is independent");
}

#[test]
fn upsert_and_returning_paths_apply_defaults_and_deduplicate_resends() {
    let path = unique_db_path("sql37-default-now-paths");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    create_events(&core);

    // UPSERT（DO UPDATE）。
    let upsert = "INSERT INTO events (id, embedding, lang) VALUES (7, '[0.1,0.2]', 'ja') \
                  ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang USING OPERATION_ID 'op-up'";
    exec(&core, "tenant-a", upsert).expect("upsert insert");
    pause();
    assert_eq!(code(&core, "tenant-a", upsert), "23505");
    let rows = select_all(
        &core,
        "tenant-a",
        "SELECT at FROM events WHERE id = 7 LIMIT 10",
    );
    assert!(matches!(rows[0][0], Cell::Timestamp(_)));

    // RETURNING 付き INSERT は評価済みの時刻を返し、再送は 23505。
    let returning = "INSERT INTO events (id, embedding, lang) VALUES (8, '[0.1,0.2]', 'ja') \
                     RETURNING at USING OPERATION_ID 'op-ret'";
    let returned = match exec(&core, "tenant-a", returning).expect("insert returning") {
        SqlOutcome::Returning(r) => ts(&r.result.rows[0].cells[0]),
        other => panic!("expected Returning, got {other:?}"),
    };
    let stored = ts(&select_all(
        &core,
        "tenant-a",
        "SELECT at FROM events WHERE id = 8 LIMIT 10",
    )[0][0]);
    assert_eq!(returned, stored);
    pause();
    assert_eq!(code(&core, "tenant-a", returning), "23505");
}

fn run_copy(core: &EngineCore, tenant: &str, sql: &str, data: &[u8]) -> Result<(), String> {
    let policy = ctx_for(tenant);
    let session = SessionState::default();
    let plan = core
        .begin_copy(&policy, &session, sql)
        .map_err(|e| e.wire_code().to_string())?;
    let mut copy_session = match plan {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected COPY FROM STDIN plan"),
    };
    copy_session
        .feed(data)
        .map_err(|e| e.wire_code().to_string())?;
    let batch = copy_session
        .finish()
        .map_err(|e| e.wire_code().to_string())?;
    core.commit_copy_in(&policy, batch)
        .map(|_| ())
        .map_err(|e| e.wire_code().to_string())
}

#[test]
fn copy_from_applies_defaults_with_one_time_and_deduplicates_resends() {
    let path = unique_db_path("sql37-default-now-copy");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    create_events(&core);
    let sql = "COPY events (id, embedding, lang) FROM STDIN USING OPERATION_ID 'op-copy'";
    let data = b"1\t[0.1,0.2]\ta\n2\t[0.1,0.2]\tb\n";
    run_copy(&core, "tenant-a", sql, data).expect("copy");
    let rows = select_all(&core, "tenant-a", "SELECT at FROM events LIMIT 10");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        ts(&rows[0][0]),
        ts(&rows[1][0]),
        "one time for the whole COPY"
    );
    let first = ts(&rows[0][0]);
    pause();
    assert_eq!(
        run_copy(&core, "tenant-a", sql, data),
        Err("23505".to_string())
    );
    let rows = select_all(&core, "tenant-a", "SELECT at FROM events LIMIT 10");
    assert!(rows.iter().all(|r| ts(&r[0]) == first));
}
// --- 分割実行 DML の固定時刻（設計 D6） ---------------------------------------------

fn create_docs(core: &EngineCore) {
    let mut session = ddl_session();
    core.execute_sql_in_session(
        &ctx_for("tenant-a"),
        &mut session,
        "CREATE TABLE docs (n BIGINT, u TEXT UNIQUE, at TIMESTAMP)",
    )
    .expect("create docs");
}

fn seed_docs(core: &EngineCore, rows: &[(u64, &str)]) {
    let values: Vec<String> = rows
        .iter()
        .map(|(id, u)| format!("({id}, {id}, '{u}', '2000-01-01 00:00:00')"))
        .collect();
    exec(
        core,
        "tenant-a",
        &format!(
            "INSERT INTO docs (id, n, u, at) VALUES {} USING OPERATION_ID 'seed'",
            values.join(", ")
        ),
    )
    .expect("seed docs");
}

fn docs_at(core: &EngineCore) -> Vec<i64> {
    select_all(core, "tenant-a", "SELECT at FROM docs LIMIT 100")
        .iter()
        .map(|r| ts(&r[0]))
        .collect()
}

const Y2000: i64 = 946_684_800_000_000;

#[test]
fn partitioned_update_uses_one_time_across_chunks_and_resume() {
    let path = unique_db_path("sql37-default-now-pdml-update");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    create_docs(&core);
    seed_docs(&core, &[(1, "a"), (2, "b")]);

    // 2 チャンク目で UNIQUE 違反にして中断させる（1 チャンク目だけ commit 済み）。
    let sql = "UPDATE docs SET at = now(), u = 'dup' WHERE n > 0 \
               USING OPERATION_ID 'op-time' PARTITIONED CHUNK 1";
    assert_eq!(code(&core, "tenant-a", sql), "VD001");
    let first_time = docs_at(&core).into_iter().max().expect("rows");
    assert!(first_time > Y2000, "chunk 1 wrote now()");

    // 原因を取り除いて同じ文を再送する。時刻はジョブ開始時の値で、残りの行にも同じ値が入る。
    exec(
        &core,
        "tenant-a",
        "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-1'",
    )
    .expect("fix the cause");
    pause();
    exec(&core, "tenant-a", sql).expect("resume completes");
    let times = docs_at(&core);
    assert_eq!(times.len(), 2);
    assert!(
        times.iter().all(|t| *t == first_time),
        "every chunk and the resumed run share one time: {times:?} vs {first_time}"
    );
    // 完了後の同じ文の再送は 23505（行は書き換わらない）。
    pause();
    assert_eq!(code(&core, "tenant-a", sql), "23505");
    assert!(docs_at(&core).iter().all(|t| *t == first_time));
}

#[test]
fn partitioned_update_time_survives_reopen_between_interruption_and_resume() {
    let path = unique_db_path("sql37-default-now-pdml-reopen");
    let _g = CleanupGuard(path.clone());
    let first_time;
    let sql = "UPDATE docs SET at = CURRENT_TIMESTAMP, u = 'dup' WHERE n > 0 \
               USING OPERATION_ID 'op-time2' PARTITIONED CHUNK 1";
    {
        let core = open_core(&path);
        create_docs(&core);
        seed_docs(&core, &[(1, "a"), (2, "b")]);
        assert_eq!(code(&core, "tenant-a", sql), "VD001");
        first_time = docs_at(&core).into_iter().max().expect("rows");
    }
    // クラッシュ復旧（再オープン）後の再送も、ジョブ記録の固定時刻で再開する。
    let core = open_core(&path);
    exec(
        &core,
        "tenant-a",
        "UPDATE docs SET u = 'fixed' WHERE id = 1 USING OPERATION_ID 'fix-1'",
    )
    .expect("fix the cause");
    pause();
    exec(&core, "tenant-a", sql).expect("resume completes");
    assert!(docs_at(&core).iter().all(|t| *t == first_time));
}

#[test]
fn partitioned_delete_accepts_transaction_fixed_functions_in_where() {
    let path = unique_db_path("sql37-default-now-pdml-delete");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    create_docs(&core);
    seed_docs(&core, &[(1, "a"), (2, "b"), (3, "c")]);
    let outcome = exec(
        &core,
        "tenant-a",
        "DELETE FROM docs WHERE at < CURRENT_TIMESTAMP USING OPERATION_ID 'd1' \
         PARTITIONED CHUNK 2",
    )
    .expect("partitioned delete with CURRENT_TIMESTAMP");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 3),
        other => panic!("expected Delete outcome, got {other:?}"),
    }
}

#[test]
fn partitioned_statement_and_clock_timestamp_are_rejected_and_atomic_set_is_accepted() {
    let path = unique_db_path("sql37-default-now-pdml-reject");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    create_docs(&core);
    seed_docs(&core, &[(1, "a")]);
    for sql in [
        "UPDATE docs SET at = statement_timestamp() WHERE n > 0 USING OPERATION_ID 'x1' \
         PARTITIONED",
        "UPDATE docs SET at = clock_timestamp() WHERE n > 0 USING OPERATION_ID 'x2' PARTITIONED",
        "UPDATE docs SET u = 'z' WHERE at < clock_timestamp() USING OPERATION_ID 'x3' \
         PARTITIONED",
        "DELETE FROM docs WHERE at < statement_timestamp() USING OPERATION_ID 'x4' PARTITIONED",
    ] {
        assert_eq!(code(&core, "tenant-a", sql), "0A000", "{sql}");
    }
    // 拒否された分割実行は副作用なし。
    assert!(docs_at(&core).iter().all(|t| *t == Y2000));
    // 原子版（分割なし）の UPDATE SET now() は受理される（Issue #2188。台帳 v3 を書く）。
    exec(
        &core,
        "tenant-a",
        "UPDATE docs SET at = now() WHERE n > 0 USING OPERATION_ID 'x5'",
    )
    .expect("atomic update set now()");
    assert!(docs_at(&core).iter().all(|t| *t != Y2000));
}
#[test]
fn file_insert_with_omitted_time_default_column_is_rejected_without_side_effects() {
    // ファイル形 INSERT は時刻 DEFAULT を評価しない（設計 D9。`bind_column_default` が常に 0A000）。
    let path = unique_db_path("sql37-default-now-file");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut session = ddl_session();
    core.execute_sql_in_session(
        &ctx_for("tenant-a"),
        &mut session,
        "CREATE TABLE notes (embedding VECTOR(2), path TEXT, body TEXT, \
         at TIMESTAMP DEFAULT now())",
    )
    .expect("create file-form table");
    assert_eq!(
        code(
            &core,
            "tenant-a",
            "INSERT INTO notes (path, body) VALUES ('a.txt', 'hello') \
             USING OPERATION_ID 'op-file'"
        ),
        "0A000"
    );
    // 副作用なし（行も台帳も残らない）。
    assert!(select_all(&core, "tenant-a", "SELECT id FROM notes LIMIT 10").is_empty());
}

#[test]
fn partitioned_delete_where_time_is_fixed_at_job_start_across_resume() {
    // WHERE だけに時刻関数を含む分割 DELETE でも、固定時刻が再開後に再評価されないこと。
    // 1 チャンク目で id 1 を削除し、2 チャンク目は FK で中断する。中断後に
    // 「最初の実行時刻より後」の行 id 9 を DEFAULT now() で追加してから再送する。
    // 固定時刻のままなら id 9 は `at < CURRENT_TIMESTAMP` に一致せず残る
    // （再評価されていれば削除される）。
    let path = unique_db_path("sql37-default-now-pdml-where");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut session = ddl_session();
    for ddl in [
        "CREATE TABLE evs (embedding VECTOR(2), n BIGINT, at TIMESTAMP DEFAULT now())",
        "CREATE TABLE kids (embedding VECTOR(2), parent_id BIGINT REFERENCES evs)",
    ] {
        core.execute_sql_in_session(&ctx_for("tenant-a"), &mut session, ddl)
            .expect(ddl);
    }
    exec(
        &core,
        "tenant-a",
        "INSERT INTO evs (id, embedding, n, at) VALUES \
         (1, '[0.1,0.2]', 1, '2000-01-01 00:00:00'), \
         (2, '[0.1,0.2]', 2, '2000-01-01 00:00:00') USING OPERATION_ID 'seed-evs'",
    )
    .expect("seed evs");
    exec(
        &core,
        "tenant-a",
        "INSERT INTO kids (id, embedding, parent_id) VALUES (1, '[0.1,0.2]', 2) \
         USING OPERATION_ID 'seed-kids'",
    )
    .expect("seed kids");

    let sql = "DELETE FROM evs WHERE at < CURRENT_TIMESTAMP USING OPERATION_ID 'd-fixed' \
               PARTITIONED CHUNK 1";
    assert_eq!(code(&core, "tenant-a", sql), "VD001");
    pause();
    exec(
        &core,
        "tenant-a",
        "INSERT INTO evs (id, embedding, n) VALUES (9, '[0.1,0.2]', 9) \
         USING OPERATION_ID 'late-row'",
    )
    .expect("late row");
    pause();
    exec(
        &core,
        "tenant-a",
        "DELETE FROM kids WHERE id = 1 USING OPERATION_ID 'kids-del'",
    )
    .expect("remove the blocker");
    exec(&core, "tenant-a", sql).expect("resume completes");
    let remaining: Vec<u64> =
        match exec(&core, "tenant-a", "SELECT id FROM evs LIMIT 10").expect("select") {
            SqlOutcome::Query(r) => r.rows.iter().map(|row| row.id).collect(),
            other => panic!("expected Query outcome, got {other:?}"),
        };
    assert_eq!(
        remaining,
        vec![9],
        "the row newer than the fixed time survives"
    );
}
