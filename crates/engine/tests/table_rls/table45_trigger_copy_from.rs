//! `COPY ... FROM STDIN` の確定処理で INSERT トリガが発火することの結合テスト（TABLE-45・SQL-32・
//! SQL-68・RECOVER-1・RLS-10。Issue #3281）。wire-server の COPY 完了処理と同じ入口
//! （`begin_copy` → `feed` → `finish` → `commit_copy_in_txn_in_session`）を、既定の invoker
//! （本番の PL/pgSQL トリガ実行）で駆動する。表はテナント SCHEMA 配下に作る。

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, InsertOutcome};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

/// テナントごとの接続相当（SCHEMA `s` を `search_path` の先頭にする）。
fn tenant_session(core: &EngineCore, tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, "CREATE SCHEMA s")
        .expect("create schema");
    core.execute_sql_in_session(&ctx(tenant), &mut s, "SET search_path TO s, public")
        .expect("set search_path");
    s
}

fn ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn ints(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> Vec<i64> {
    let mut v: Vec<i64> = match ok(core, tenant, s, sql) {
        SqlOutcome::Query(q) => q
            .rows
            .into_iter()
            .filter_map(|r| r.cells.into_iter().next())
            .map(|c| match c {
                Cell::SignedInteger(v) => v,
                Cell::Integer(v) => i64::try_from(v).expect("fits"),
                other => panic!("unexpected {other:?}"),
            })
            .collect(),
        other => panic!("expected query, got {other:?}"),
    };
    v.sort_unstable();
    v
}

/// 共有 `public` の監査表 `audit (n, lv)`。トリガ本体は SCHEMA 修飾の書き込み先を受けないため、
/// 監査は未修飾名（`search_path` の末尾の `public`）へ書く。行は書いたテナントのものだけが見える。
fn audit_table(core: &EngineCore) {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &ctx("t1"),
        &mut s,
        "CREATE TABLE audit (n INTEGER, lv TEXT)",
    )
    .expect("create audit");
}

/// 表 `s.tt (n)` を作る。
fn setup(core: &EngineCore, tenant: &str, s: &mut SessionState) {
    ok(core, tenant, s, "CREATE TABLE s.tt (n INTEGER)");
}

fn func(core: &EngineCore, tenant: &str, s: &mut SessionState, name: &str, body: &str) {
    ok(
        core,
        tenant,
        s,
        &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
    );
}

fn trig(core: &EngineCore, tenant: &str, s: &mut SessionState, spec: &str, f: &str, n: &str) {
    let (when, level) = spec.split_once(" FOR ").expect("spec has FOR");
    ok(
        core,
        tenant,
        s,
        &format!("CREATE TRIGGER {n} {when} ON s.tt FOR {level} EXECUTE FUNCTION {f}()"),
    );
}

/// BEFORE ROW（2 は NULL でスキップ・他は +100）・AFTER ROW（監査）・AFTER STATEMENT（監査）を付ける。
fn all_triggers(core: &EngineCore, tenant: &str, s: &mut SessionState) {
    setup(core, tenant, s);
    func(
        core,
        tenant,
        s,
        "bump",
        "BEGIN IF NEW.n = 2 THEN RETURN NULL; END IF; NEW.n := NEW.n + 100; RETURN NEW; END",
    );
    func(
        core,
        tenant,
        s,
        "after_row",
        "BEGIN INSERT INTO audit (n, lv) VALUES (NEW.n, TG_LEVEL); RETURN NULL; END",
    );
    func(
        core,
        tenant,
        s,
        "stmt",
        "BEGIN INSERT INTO audit (n, lv) VALUES (0, TG_LEVEL); RETURN NULL; END",
    );
    trig(core, tenant, s, "BEFORE INSERT FOR EACH ROW", "bump", "tb");
    trig(
        core,
        tenant,
        s,
        "AFTER INSERT FOR EACH ROW",
        "after_row",
        "ta",
    );
    trig(
        core,
        tenant,
        s,
        "AFTER INSERT FOR EACH STATEMENT",
        "stmt",
        "ts",
    );
}

/// COPY を wire-server と同じ入口で確定する（autocommit）。
fn copy(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
    data: &[u8],
) -> Result<InsertOutcome, SqlSurfaceError> {
    let mut txn = core.new_session_transaction();
    copy_in(core, tenant, s, &mut txn, sql, data)
}

fn copy_in<'e>(
    core: &'e EngineCore,
    tenant: &str,
    s: &mut SessionState,
    txn: &mut engine::sql::transaction::SessionTransaction<'e>,
    sql: &str,
    data: &[u8],
) -> Result<InsertOutcome, SqlSurfaceError> {
    let policy = ctx(tenant);
    let plan = core.begin_copy_in_txn(&policy, s, txn, sql)?;
    let CopyPlan::From(mut cs) = plan else {
        panic!("expected COPY FROM STDIN plan");
    };
    cs.feed(data)?;
    let batch = cs.finish()?;
    core.commit_copy_in_txn_in_session(&policy, s, txn, batch)
}

// 変更前は INSERT トリガのある表への COPY が `0A000` だった。いまは発火し、BEFORE の書き換え・
// NULL スキップ・AFTER ROW・STATEMENT が複数行 INSERT と同じ結果になる。
#[test]
fn copy_fires_all_trigger_kinds_same_as_insert() {
    let (_g, core) = open("t3281-same");
    audit_table(&core);
    let mut a = tenant_session(&core, "t1");
    all_triggers(&core, "t1", &mut a);
    let out = copy(&core, "t1", &mut a, "COPY tt (n) FROM STDIN", b"1\n2\n3\n")
        .expect("COPY fires triggers");
    assert_eq!(out.rows_affected, 2);
    assert_eq!(
        ints(&core, "t1", &mut a, "SELECT n FROM tt"),
        vec![101, 103]
    );
    // AFTER ROW ×2（NULL スキップした行は無し）＋ STATEMENT ×1。
    assert_eq!(
        ints(&core, "t1", &mut a, "SELECT n FROM audit"),
        vec![0, 101, 103]
    );

    // 比較: 別テナントで同じ定義へ複数行 INSERT。
    let mut b = tenant_session(&core, "t9");
    all_triggers(&core, "t9", &mut b);
    match ok(
        &core,
        "t9",
        &mut b,
        "INSERT INTO tt (n) VALUES (1), (2), (3)",
    ) {
        SqlOutcome::Insert(o) => assert_eq!(o.rows_affected, out.rows_affected),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        ints(&core, "t9", &mut b, "SELECT n FROM tt"),
        ints(&core, "t1", &mut a, "SELECT n FROM tt")
    );
    assert_eq!(
        ints(&core, "t9", &mut b, "SELECT n FROM audit"),
        ints(&core, "t1", &mut a, "SELECT n FROM audit")
    );
}

// CSV でも発火する。
#[test]
fn copy_csv_fires_triggers() {
    let (_g, core) = open("t3281-csv");
    audit_table(&core);
    let mut a = tenant_session(&core, "t1");
    all_triggers(&core, "t1", &mut a);
    copy(
        &core,
        "t1",
        &mut a,
        "COPY tt (n) FROM STDIN WITH (FORMAT csv)",
        b"5\n7\n",
    )
    .expect("csv copy");
    assert_eq!(
        ints(&core, "t1", &mut a, "SELECT n FROM tt"),
        vec![105, 107]
    );
}

// トリガ関数内のエラーで COPY 全体が失敗し、行・トリガが書いた行・台帳のどれも残らない。
#[test]
fn trigger_error_aborts_whole_copy_and_resend_succeeds() {
    let (_g, core) = open("t3281-atomic");
    audit_table(&core);
    let mut a = tenant_session(&core, "t1");
    setup(&core, "t1", &mut a);
    func(
        &core,
        "t1",
        &mut a,
        "boom",
        "BEGIN INSERT INTO audit (n, lv) VALUES (NEW.n, 'x'); IF NEW.n = 3 THEN RAISE EXCEPTION 'boom'; END IF; RETURN NEW; END",
    );
    trig(
        &core,
        "t1",
        &mut a,
        "AFTER INSERT FOR EACH ROW",
        "boom",
        "ta",
    );
    let sql = "COPY tt (n) FROM STDIN USING OPERATION_ID 'op-3281'";
    let err = copy(&core, "t1", &mut a, sql, b"1\n2\n3\n").expect_err("trigger error fails COPY");
    assert_ne!(err.wire_code(), "23505");
    assert_ne!(err.wire_code(), "0A000");
    assert!(ints(&core, "t1", &mut a, "SELECT n FROM tt").is_empty());
    assert!(ints(&core, "t1", &mut a, "SELECT n FROM audit").is_empty());
    // 台帳が残っていないので、トリガを外した後の同じ operation_id の再送は成功する。
    ok(&core, "t1", &mut a, "DROP TRIGGER ta ON s.tt");
    copy(&core, "t1", &mut a, sql, b"1\n2\n3\n").expect("resend succeeds");
    assert_eq!(ints(&core, "t1", &mut a, "SELECT n FROM tt"), vec![1, 2, 3]);
}

// 成功後の同じ operation_id の再送は `23505` で、トリガは再発火しない。
#[test]
fn resend_is_23505_without_refire() {
    let (_g, core) = open("t3281-resend");
    audit_table(&core);
    let mut a = tenant_session(&core, "t1");
    all_triggers(&core, "t1", &mut a);
    let sql = "COPY tt (n) FROM STDIN USING OPERATION_ID 'op-r'";
    copy(&core, "t1", &mut a, sql, b"1\n3\n").expect("first");
    let err = copy(&core, "t1", &mut a, sql, b"1\n3\n").expect_err("resend");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(
        ints(&core, "t1", &mut a, "SELECT n FROM audit"),
        vec![0, 101, 103]
    );
}

// 明示 Tx: COMMIT で行とトリガの結果が残り、ROLLBACK では残らない。トリガのエラーで Tx は Failed になる。
#[test]
fn explicit_transaction_commit_rollback_and_failed() {
    let (_g, core) = open("t3281-txn");
    audit_table(&core);
    let mut a = tenant_session(&core, "t1");
    all_triggers(&core, "t1", &mut a);
    let c = ctx("t1");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "BEGIN")
        .expect("begin");
    copy_in(
        &core,
        "t1",
        &mut a,
        &mut txn,
        "COPY tt (n) FROM STDIN",
        b"1\n",
    )
    .expect("copy in txn");
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "COMMIT")
        .expect("commit");
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "BEGIN")
        .expect("begin");
    copy_in(
        &core,
        "t1",
        &mut a,
        &mut txn,
        "COPY tt (n) FROM STDIN",
        b"3\n",
    )
    .expect("copy in txn");
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(ints(&core, "t1", &mut a, "SELECT n FROM tt"), vec![101]);
    assert_eq!(
        ints(&core, "t1", &mut a, "SELECT n FROM audit"),
        vec![0, 101]
    );

    // トリガのエラーで Tx が Failed になる。
    func(
        &core,
        "t1",
        &mut a,
        "boom",
        "BEGIN RAISE EXCEPTION 'boom'; RETURN NEW; END",
    );
    trig(
        &core,
        "t1",
        &mut a,
        "BEFORE INSERT FOR EACH STATEMENT",
        "boom",
        "tx",
    );
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "BEGIN")
        .expect("begin");
    copy_in(
        &core,
        "t1",
        &mut a,
        &mut txn,
        "COPY tt (n) FROM STDIN",
        b"4\n",
    )
    .expect_err("trigger error");
    let e = core
        .execute_sql_in_txn(&c, &mut a, &mut txn, "SELECT n FROM tt")
        .expect_err("failed txn");
    assert_eq!(e.wire_code(), "25P02");
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "ROLLBACK")
        .expect("rollback");
}

// 越境: t1 だけにトリガを付ける。t2 の COPY では何も発火せず、t1 の COPY は t1 の監査だけに書く。
#[test]
fn trigger_does_not_cross_tenants() {
    let (_g, core) = open("t3281-tenant");
    audit_table(&core);
    let mut a = tenant_session(&core, "t1");
    let mut b = tenant_session(&core, "t2");
    setup(&core, "t2", &mut b);
    all_triggers(&core, "t1", &mut a);
    copy(&core, "t2", &mut b, "COPY tt (n) FROM STDIN", b"1\n3\n").expect("t2 copy");
    assert_eq!(ints(&core, "t2", &mut b, "SELECT n FROM tt"), vec![1, 3]);
    assert!(ints(&core, "t1", &mut a, "SELECT n FROM audit").is_empty());
    copy(&core, "t1", &mut a, "COPY tt (n) FROM STDIN", b"1\n").expect("t1 copy");
    assert_eq!(
        ints(&core, "t1", &mut a, "SELECT n FROM audit"),
        vec![0, 101]
    );
    assert!(ints(&core, "t2", &mut b, "SELECT n FROM audit").is_empty());
    assert_eq!(ints(&core, "t2", &mut b, "SELECT n FROM tt"), vec![1, 3]);
}

// 窓口の許可リストに無い本体（`INSERT ... SELECT`）は `0A000` で、残骸を残さない。
// 本体の `DELETE`・表を読む `SELECT` は Issue #3187 で対応済み。
//
// intentional: docs/design/plpgsql-subset.md
#[test]
fn unsupported_body_is_0a000_without_residue() {
    let (_g, core) = open("t3281-unsupported");
    audit_table(&core);
    let mut a = tenant_session(&core, "t1");
    setup(&core, "t1", &mut a);
    func(
        &core,
        "t1",
        &mut a,
        "upd",
        "BEGIN INSERT INTO audit (n) VALUES (50); INSERT INTO audit (n) SELECT 51; RETURN NEW; END",
    );
    trig(
        &core,
        "t1",
        &mut a,
        "BEFORE INSERT FOR EACH ROW",
        "upd",
        "tb",
    );
    let e = copy(&core, "t1", &mut a, "COPY tt (n) FROM STDIN", b"1\n").expect_err("0A000");
    assert_eq!(e.wire_code(), "0A000");
    assert!(ints(&core, "t1", &mut a, "SELECT n FROM tt").is_empty());
    assert!(ints(&core, "t1", &mut a, "SELECT n FROM audit").is_empty());
}

// セッションなしの公開入口は主体の写しを張れないため、トリガのある表は `0A000`（fail-closed）で残骸なし。
//
// intentional: docs/design/trigger-execution-model.md
#[test]
fn sessionless_entry_stays_0a000() {
    let (_g, core) = open("t3281-sessionless");
    audit_table(&core);
    let mut a = tenant_session(&core, "t1");
    all_triggers(&core, "t1", &mut a);
    let policy = ctx("t1");
    let CopyPlan::From(mut cs) = core
        .begin_copy(&policy, &a, "COPY tt (n) FROM STDIN")
        .expect("plan")
    else {
        panic!("from");
    };
    cs.feed(b"1\n").expect("feed");
    let batch = cs.finish().expect("finish");
    let e = core.commit_copy_in(&policy, batch).expect_err("0A000");
    assert_eq!(e.wire_code(), "0A000");
    assert!(ints(&core, "t1", &mut a, "SELECT n FROM tt").is_empty());
}

/// 本体が `now()` を `audit_ts` へ記録する BEFORE ROW トリガを付ける（Issue #3281・SQL-37）。
fn clock_trigger(core: &EngineCore, tenant: &str, s: &mut SessionState) {
    setup(core, tenant, s);
    // 監査表は共有 `public`（トリガ本体は SCHEMA 修飾の書き込み先を受けない）へ作る。
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(
        &ctx(tenant),
        &mut ddl,
        "CREATE TABLE audit_ts (ts TIMESTAMP)",
    )
    .expect("create audit_ts");
    func(
        core,
        tenant,
        s,
        "clk",
        "BEGIN INSERT INTO audit_ts (ts) VALUES (now()); RETURN NEW; END",
    );
    trig(core, tenant, s, "BEFORE INSERT FOR EACH ROW", "clk", "tc");
}

fn ts_cells(core: &EngineCore, tenant: &str, s: &mut SessionState) -> Vec<Cell> {
    match ok(core, tenant, s, "SELECT ts FROM audit_ts") {
        SqlOutcome::Query(q) => q
            .rows
            .into_iter()
            .filter_map(|r| r.cells.into_iter().next())
            .collect(),
        other => panic!("expected query, got {other:?}"),
    }
}

// 複数行 COPY のトリガ本体の `now()` は全行で同一（COPY 開始時に確定した文時刻を共有する）。
#[test]
fn copy_trigger_now_is_same_for_all_rows() {
    let (_g, core) = open("t3281-clock");
    let mut a = tenant_session(&core, "t1");
    clock_trigger(&core, "t1", &mut a);
    let rows: String = (0..20).map(|i| format!("{i}\n")).collect();
    copy(
        &core,
        "t1",
        &mut a,
        "COPY tt (n) FROM STDIN",
        rows.as_bytes(),
    )
    .expect("copy");
    let ts = ts_cells(&core, "t1", &mut a);
    assert_eq!(ts.len(), 20);
    assert!(ts.iter().all(|c| c == &ts[0]), "now() must be shared");
}

// 明示 Tx の COPY のトリガ本体の `now()` は BEGIN 時点（トランザクション開始時刻）になる。
#[test]
fn copy_trigger_now_is_txn_start_in_explicit_txn() {
    let (_g, core) = open("t3281-clock-txn");
    let mut a = tenant_session(&core, "t1");
    clock_trigger(&core, "t1", &mut a);
    let c = ctx("t1");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "BEGIN")
        .expect("begin");
    let begin_now = match core
        .execute_sql_in_txn(&c, &mut a, &mut txn, "SELECT now()")
        .expect("now")
    {
        SqlOutcome::Query(q) => q.rows[0].cells[0].clone(),
        other => panic!("expected query, got {other:?}"),
    };
    std::thread::sleep(std::time::Duration::from_millis(20));
    copy_in(
        &core,
        "t1",
        &mut a,
        &mut txn,
        "COPY tt (n) FROM STDIN",
        b"1\n2\n3\n",
    )
    .expect("copy in txn");
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "COMMIT")
        .expect("commit");
    let ts = ts_cells(&core, "t1", &mut a);
    assert_eq!(ts.len(), 3);
    assert!(
        ts.iter().all(|c| c == &begin_now),
        "now() must be BEGIN time"
    );
}

/// NOT NULL 列 `b` を持つ表 `s.nn (a, b)` に BEFORE ROW INSERT トリガ（`fill` が真なら `b` を 7 で補う）を付ける。
fn not_null_table(core: &EngineCore, tenant: &str, s: &mut SessionState, fill: bool) {
    ok(
        core,
        tenant,
        s,
        "CREATE TABLE s.nn (a INTEGER NOT NULL, b INTEGER NOT NULL)",
    );
    let body = if fill {
        "BEGIN IF NEW.b IS NULL THEN NEW.b := 7; END IF; RETURN NEW; END"
    } else {
        "BEGIN RETURN NEW; END"
    };
    func(core, tenant, s, "fillb", body);
    ok(
        core,
        tenant,
        s,
        "CREATE TRIGGER tn BEFORE INSERT ON s.nn FOR EACH ROW EXECUTE FUNCTION fillb()",
    );
}

// BEFORE ROW が NOT NULL 列を補う表では、`\N`・列省略のどちらも束縛・列リスト検査で `23502` にならず、
// トリガ適用後の値で書かれる（Issue #3281。INSERT の省略列の遅延と同じ契約）。
#[test]
fn before_trigger_fills_not_null_for_null_marker_and_omitted_column() {
    let (_g, core) = open("t3281-nn-fill");
    let mut a = tenant_session(&core, "t1");
    not_null_table(&core, "t1", &mut a, true);
    copy(
        &core,
        "t1",
        &mut a,
        "COPY nn (a, b) FROM STDIN",
        b"1\t\\N\n2\t5\n",
    )
    .expect("null marker filled by BEFORE trigger");
    copy(&core, "t1", &mut a, "COPY nn (a) FROM STDIN", b"3\n")
        .expect("omitted column filled by BEFORE trigger");
    assert_eq!(ints(&core, "t1", &mut a, "SELECT a FROM nn"), vec![1, 2, 3]);
    assert_eq!(ints(&core, "t1", &mut a, "SELECT b FROM nn"), vec![5, 7, 7]);
}

// BEFORE ROW が補わない場合は最終 NEW の検査で `23502` になり、行は残らない。
#[test]
fn before_trigger_not_filling_not_null_is_23502_without_residue() {
    let (_g, core) = open("t3281-nn-nofill");
    let mut a = tenant_session(&core, "t1");
    not_null_table(&core, "t1", &mut a, false);
    let e1 = copy(
        &core,
        "t1",
        &mut a,
        "COPY nn (a, b) FROM STDIN",
        b"1\t\\N\n",
    )
    .expect_err("null marker not filled");
    assert_eq!(e1.wire_code(), "23502");
    let e2 = copy(&core, "t1", &mut a, "COPY nn (a) FROM STDIN", b"1\n")
        .expect_err("omitted not filled");
    assert_eq!(e2.wire_code(), "23502");
    assert!(ints(&core, "t1", &mut a, "SELECT a FROM nn").is_empty());
}

// トリガの無い表は従来どおり束縛時に `23502`。
#[test]
fn without_trigger_not_null_is_still_23502() {
    let (_g, core) = open("t3281-nn-plain");
    let mut a = tenant_session(&core, "t1");
    ok(
        &core,
        "t1",
        &mut a,
        "CREATE TABLE s.nn (a INTEGER NOT NULL, b INTEGER NOT NULL)",
    );
    let e1 = copy(
        &core,
        "t1",
        &mut a,
        "COPY nn (a, b) FROM STDIN",
        b"1\t\\N\n",
    )
    .expect_err("null marker");
    assert_eq!(e1.wire_code(), "23502");
    let e2 = copy(&core, "t1", &mut a, "COPY nn (a) FROM STDIN", b"1\n").expect_err("omitted");
    assert_eq!(e2.wire_code(), "23502");
}

// writer を取得済みの明示 Tx（先行の書き込み後）でも、COPY の NOT NULL 検査の遅延は writer 側のトリガ参照で
// 判定され、INSERT と同じく BEFORE ROW が補える（Issue #3281）。なお `CREATE TRIGGER` 自体は明示 Tx では
// 未対応のため、同一 Tx で作ったトリガが COPY に見えない状況は現状では起こらない（writer 側参照は将来の
// 明示 Tx 内 DDL 対応でも食い違わないための備え）。
#[test]
fn writer_txn_before_trigger_defers_not_null_for_copy() {
    let (_g, core) = open("t3281-nn-writer");
    audit_table(&core);
    let mut a = tenant_session(&core, "t1");
    not_null_table(&core, "t1", &mut a, true);
    let c = ctx("t1");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "INSERT INTO audit (n) VALUES (9)")
        .expect("first write acquires the writer");
    copy_in(
        &core,
        "t1",
        &mut a,
        &mut txn,
        "COPY nn (a, b) FROM STDIN",
        b"1\t\\N\n",
    )
    .expect("null marker filled by BEFORE trigger");
    copy_in(
        &core,
        "t1",
        &mut a,
        &mut txn,
        "COPY nn (a) FROM STDIN",
        b"2\n",
    )
    .expect("omitted column filled by BEFORE trigger");
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(ints(&core, "t1", &mut a, "SELECT a FROM nn"), vec![1, 2]);
    assert_eq!(ints(&core, "t1", &mut a, "SELECT b FROM nn"), vec![7, 7]);
}
