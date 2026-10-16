//! 書き込み文の現在時刻関数と台帳 v3 の結合テスト（Issue #2188、対象ビヘイビア: SQL-37・
//! RECOVER-10・RECOVER-11・RECOVER-12。ポインタ: `docs/design/now-evaluated-value-ledger.md`
//! D4・D5・D11）。
//!
//! 役割: `EngineCore` の production 経路（`execute_sql_in_session`／`execute_sql_in_txn`）で
//! 書き込み文を実行し、(1) 評価時点の関係（`now()` はトランザクション固定・
//! `statement_timestamp()` は文ごと・`clock_timestamp()` は呼び出しごと）、(2) 時刻関数を
//! 使った書き込みだけが v3 エントリ（トランザクション開始時刻つき）を書くこと、
//! (3) 同じ文の再送が評価時刻によらず `23505` になり行が初回の値のまま残ること、
//! (4) 再オープン後も同じであることを検証する。台帳の中身は `Storage::db()`（クレート内限定）
//! から `recovery::ledger::evaluated_at_in_read_txn` で読む。
//! 時刻の絶対値には依存せず、同値・単調性・エラーコードだけを見る。

use std::time::Duration;

use redb::ReadableDatabase;

use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::recovery::ledger::evaluated_at_in_read_txn;
use crate::recovery::required_op_id::OperationId;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn setup(core: &EngineCore) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE TABLE ev (embedding VECTOR(2), n TIMESTAMP, s TIMESTAMP, c TIMESTAMP, \
         d TIMESTAMP DEFAULT now(), lt TIME)",
    )
    .expect("create table");
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn rows(core: &EngineCore, sql: &str) -> Vec<Vec<Cell>> {
    match run(core, "tenant-a", sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|r| r.cells).collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn ts(c: &Cell) -> i64 {
    match c {
        Cell::Timestamp(v) => *v,
        other => panic!("expected Timestamp, got {other:?}"),
    }
}

fn pause() {
    std::thread::sleep(Duration::from_millis(15));
}

/// 台帳に記録された評価時刻（v3 以外・未記録は `None`）。`core` を drop 済みのパスから読む。
fn ledger_time(path: &std::path::Path, op: &str) -> Option<i64> {
    let storage = Storage::open(path).expect("reopen storage");
    let read = storage.db().begin_read().expect("begin read");
    let id = OperationId::parse(op).expect("operation id");
    evaluated_at_in_read_txn(&read, "tenant-a", "ev", &id).expect("ledger read")
}

#[test]
fn explicit_txn_time_functions_follow_pg_evaluation_points() {
    let path = unique_db_path("write-time-txn");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    setup(&core);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx("tenant-a");
    for sql in [
        "BEGIN",
        "INSERT INTO ev (id, embedding, n, s, c) VALUES \
         (1, '[0.1,0.2]', now(), statement_timestamp(), clock_timestamp()) \
         USING OPERATION_ID 'w-1'",
    ] {
        core.execute_sql_in_txn(&c, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    pause();
    core.execute_sql_in_txn(
        &c,
        &mut session,
        &mut txn,
        "INSERT INTO ev (id, embedding, n, s, c) VALUES \
         (2, '[0.1,0.2]', now(), statement_timestamp(), clock_timestamp()), \
         (3, '[0.1,0.2]', now(), statement_timestamp(), clock_timestamp()) \
         USING OPERATION_ID 'w-2'",
    )
    .expect("multi row insert");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    let r = rows(&core, "SELECT n, s, c FROM ev LIMIT 10");
    assert_eq!(r.len(), 3);
    let by_id = |id: u64| -> (i64, i64, i64) {
        let r = rows(
            &core,
            &format!("SELECT n, s, c FROM ev WHERE id = {id} LIMIT 10"),
        );
        (ts(&r[0][0]), ts(&r[0][1]), ts(&r[0][2]))
    };
    let (n1, s1, c1) = by_id(1);
    let (n2, s2, c2) = by_id(2);
    let (n3, s3, c3) = by_id(3);
    // now() はトランザクション内で固定。
    assert!(n1 == n2 && n2 == n3);
    // statement_timestamp() は文ごと: 同一文の行は同値、文をまたぐと進む。
    assert_eq!(s2, s3);
    assert!(s2 > s1);
    // clock_timestamp() は呼び出しごと: 非減少で、文の時刻以上。
    assert!(c1 >= s1 && c2 >= s2 && c3 >= c2);
    assert!(c2 > c1);
    drop(core);
    // 台帳は時刻関数を使った 2 文とも v3（トランザクション開始時刻 = now()）。
    assert_eq!(ledger_time(&path, "w-1"), Some(n1));
    assert_eq!(ledger_time(&path, "w-2"), Some(n1));
}

#[test]
fn update_set_and_where_evaluate_per_row_and_record_v3_only_when_used() {
    let path = unique_db_path("write-time-update");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    setup(&core);
    for id in 1..=3 {
        run(
            &core,
            "tenant-a",
            &format!(
                "INSERT INTO ev (id, embedding, n, d) VALUES ({id}, '[0.1,0.2]', \
                 '2000-01-01 00:00:00', '2000-01-01 00:00:00') USING OPERATION_ID 'plain-{id}'"
            ),
        )
        .expect("plain insert");
    }
    run(
        &core,
        "tenant-a",
        "UPDATE ev SET c = clock_timestamp() WHERE n < now() USING OPERATION_ID 'u-1'",
    )
    .expect("update with time functions");
    let r = rows(&core, "SELECT c FROM ev LIMIT 10");
    let mut cs: Vec<i64> = r.iter().map(|row| ts(&row[0])).collect();
    assert_eq!(cs.len(), 3);
    cs.sort_unstable();
    assert!(cs[0] > 1_700_000_000_000_000);
    // 時刻関数を使わない文は v2 のまま（評価時刻を持たない）。
    run(
        &core,
        "tenant-a",
        "UPDATE ev SET n = '2001-01-01 00:00:00' WHERE id = 1 USING OPERATION_ID 'u-2'",
    )
    .expect("plain update");
    drop(core);
    assert!(ledger_time(&path, "u-1").is_some());
    assert_eq!(ledger_time(&path, "u-2"), None);
    assert_eq!(ledger_time(&path, "plain-1"), None);
}

#[test]
fn resend_returns_23505_keeps_first_values_and_survives_reopen() {
    let path = unique_db_path("write-time-resend");
    let _g = CleanupGuard(path.clone());
    let sql = "INSERT INTO ev (id, embedding, n, s, c, lt) VALUES \
               (1, '[0.1,0.2]', now(), statement_timestamp(), clock_timestamp(), LOCALTIME) \
               USING OPERATION_ID 'r-1'";
    let (first, evaluated_at) = {
        let core = open_core(&path);
        setup(&core);
        run(&core, "tenant-a", sql).expect("first insert");
        let first = rows(&core, "SELECT n, s, c, d, lt FROM ev WHERE id = 1 LIMIT 10");
        pause();
        // 評価時刻が変わっても同じ文の再送は 23505 で、行は書き換わらない。
        assert_eq!(run(&core, "tenant-a", sql).expect_err("resend"), "23505");
        // 形の違う文（now() をリテラルに替える）は 22023。
        let other = "INSERT INTO ev (id, embedding, n, s, c, lt) VALUES \
                     (1, '[0.1,0.2]', '2000-01-01 00:00:00', statement_timestamp(), \
                     clock_timestamp(), LOCALTIME) USING OPERATION_ID 'r-1'";
        assert_eq!(
            run(&core, "tenant-a", other).expect_err("mismatch"),
            "22023"
        );
        // now() と CURRENT_TIMESTAMP は別の構文形（設計 D3）。
        let alias = "INSERT INTO ev (id, embedding, n, s, c, lt) VALUES \
                     (1, '[0.1,0.2]', CURRENT_TIMESTAMP, statement_timestamp(), \
                     clock_timestamp(), LOCALTIME) USING OPERATION_ID 'r-1'";
        assert_eq!(run(&core, "tenant-a", alias).expect_err("alias"), "22023");
        // 他テナントの同じ operation_id は独立（行は別テナントのものとして書ける）。
        run(&core, "tenant-b", sql).expect("other tenant is independent");
        let after = rows(&core, "SELECT n, s, c, d, lt FROM ev WHERE id = 1 LIMIT 10");
        assert_eq!(first, after);
        let n = ts(&first[0][0]);
        (first, n)
    };
    assert_eq!(ledger_time(&path, "r-1"), Some(evaluated_at));
    // クラッシュ復旧相当（閉じて再オープン）後も値は初回と一致し、再送は 23505。
    let core = open_core(&path);
    let again = rows(&core, "SELECT n, s, c, d, lt FROM ev WHERE id = 1 LIMIT 10");
    assert_eq!(first, again);
    assert_eq!(run(&core, "tenant-a", sql).expect_err("resend"), "23505");
}

#[test]
fn rollback_leaves_no_row_and_no_ledger_entry() {
    let path = unique_db_path("write-time-rollback");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    setup(&core);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx("tenant-a");
    for sql in [
        "BEGIN",
        "INSERT INTO ev (id, embedding, n) VALUES (1, '[0.1,0.2]', now()) \
         USING OPERATION_ID 'rb-1'",
        "ROLLBACK",
    ] {
        core.execute_sql_in_txn(&c, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    assert!(rows(&core, "SELECT n FROM ev LIMIT 10").is_empty());
    drop(core);
    assert_eq!(ledger_time(&path, "rb-1"), None);
    // 台帳エントリが無いので、同じ operation_id で改めて書ける。
    let core = open_core(&path);
    run(
        &core,
        "tenant-a",
        "INSERT INTO ev (id, embedding, n) VALUES (1, '[0.1,0.2]', now()) \
         USING OPERATION_ID 'rb-1'",
    )
    .expect("insert after rollback");
}

#[test]
fn default_application_records_v3_and_values_time_differs_from_default() {
    let path = unique_db_path("write-time-default");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    setup(&core);
    // DEFAULT now() の適用（既存の #1587 経路）にも評価時刻が付く。
    run(
        &core,
        "tenant-a",
        "INSERT INTO ev (id, embedding) VALUES (1, '[0.1,0.2]') USING OPERATION_ID 'd-1'",
    )
    .expect("insert with default");
    // VALUES の now()（種別 1）と DEFAULT の now()（種別 2）は別ハッシュ。
    assert_eq!(
        run(
            &core,
            "tenant-a",
            "INSERT INTO ev (id, embedding, d) VALUES (1, '[0.1,0.2]', now()) \
             USING OPERATION_ID 'd-1'"
        )
        .expect_err("different syntax"),
        "22023"
    );
    drop(core);
    assert!(ledger_time(&path, "d-1").is_some());
}

#[test]
fn new_time_defaults_evaluate_per_statement_and_per_row() {
    let path = unique_db_path("write-time-new-defaults");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE TABLE ev (embedding VECTOR(2), n TIMESTAMP, \
         ds TIMESTAMP DEFAULT statement_timestamp(), dc TIMESTAMP DEFAULT clock_timestamp(), \
         dl TIME DEFAULT LOCALTIME(3))",
    )
    .expect("create table");
    run(
        &core,
        "tenant-a",
        "INSERT INTO ev (id, embedding, n) VALUES (1, '[0.1,0.2]', now()), \
         (2, '[0.1,0.2]', now()), (3, '[0.1,0.2]', now()) USING OPERATION_ID 'nd-1'",
    )
    .expect("multi row insert with new defaults");
    let r = rows(&core, "SELECT n, ds, dc, dl FROM ev LIMIT 10");
    assert_eq!(r.len(), 3);
    // statement_timestamp() は同一文で同値（autocommit では now() と同値）、
    // clock_timestamp() は行ごとに評価されて非減少。
    for row in &r {
        assert_eq!(ts(&row[0]), ts(&row[1]));
        assert!(ts(&row[2]) >= ts(&row[1]));
        assert!(matches!(row[3], Cell::Time(t) if (0..86_400_000_000).contains(&t)));
    }
    let mut clocks: Vec<i64> = r.iter().map(|row| ts(&row[2])).collect();
    clocks.sort_unstable();
    assert_eq!(clocks.len(), 3);
    // 時刻 DEFAULT 付き列の ADD COLUMN は引き続き拒否される（設計 D10）。
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "ALTER TABLE ev ADD COLUMN x TIMESTAMP DEFAULT clock_timestamp()",
        )
        .expect_err("add column with clock default");
    assert!(
        matches!(err.wire_code(), "0A000" | "42601"),
        "{}",
        err.wire_code()
    );
}

#[test]
fn partitioned_completion_records_the_fixed_time_in_v3() {
    let path = unique_db_path("write-time-partitioned");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    setup(&core);
    for id in 1..=3 {
        run(
            &core,
            "tenant-a",
            &format!(
                "INSERT INTO ev (id, embedding, n, d) VALUES ({id}, '[0.1,0.2]', \
                 '2000-01-01 00:00:00', '2000-01-01 00:00:00') USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .expect("seed");
    }
    run(
        &core,
        "tenant-a",
        "UPDATE ev SET s = now() WHERE n < now() USING OPERATION_ID 'pt-1' PARTITIONED",
    )
    .expect("partitioned update with now()");
    let r = rows(&core, "SELECT s FROM ev LIMIT 10");
    assert_eq!(r.len(), 3);
    let fixed = ts(&r[0][0]);
    assert!(r.iter().all(|row| ts(&row[0]) == fixed));
    drop(core);
    assert_eq!(ledger_time(&path, "pt-1"), Some(fixed));
}

fn run_copy(core: &EngineCore, sql: &str, data: &[u8]) {
    let session = SessionState::default();
    let plan = core
        .begin_copy(&ctx("tenant-a"), &session, sql)
        .expect("begin copy");
    let mut copy = match plan {
        crate::core::CopyPlan::From(s) => s,
        crate::core::CopyPlan::To(..) => panic!("expected COPY FROM STDIN plan"),
    };
    copy.feed(data).expect("feed");
    let batch = copy.finish().expect("finish");
    core.commit_copy_in(&ctx("tenant-a"), batch)
        .expect("commit copy");
}

// Issue #2188（PR #2464 レビュー対応）: COPY は束縛と台帳記録が別スコープに分かれるため、
// DEFAULT now() を評価した COPY だけが v3 を残し、時刻を使わない COPY・同一スレッドの
// 後続 INSERT は v2 のまま（COPY の評価が文の外へ漏れない）。
#[test]
fn copy_records_v3_only_when_time_default_is_evaluated_and_does_not_leak() {
    let path = unique_db_path("write-time-copy");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    setup(&core);
    run_copy(
        &core,
        "COPY ev (id, embedding) FROM STDIN USING OPERATION_ID 'cp-1'",
        b"1\t[0.1,0.2]\n",
    );
    run_copy(
        &core,
        "COPY ev (id, embedding, d) FROM STDIN USING OPERATION_ID 'cp-2'",
        b"2\t[0.1,0.2]\t2000-01-01 00:00:00\n",
    );
    run(
        &core,
        "tenant-a",
        "INSERT INTO ev (id, embedding, d) VALUES (3, '[0.1,0.2]', '2000-01-01 00:00:00') \
         USING OPERATION_ID 'cp-3'",
    )
    .expect("plain insert");
    drop(core);
    assert!(ledger_time(&path, "cp-1").is_some());
    assert_eq!(ledger_time(&path, "cp-2"), None);
    assert_eq!(ledger_time(&path, "cp-3"), None);
}
