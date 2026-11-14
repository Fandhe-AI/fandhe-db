//! 本番の `PlpgsqlTriggerInvoker`（`EngineCore` の既定 invoker）でのトリガ発火の、越境・台帳・fail-closed の
//! 網羅的な結合テスト（TABLE-45・SQL-68・RLS-10・RECOVER-10〜12 のポインタ。Issue #3188）。
//!
//! 注入 invoker（`with_trigger_invoker_for_test`）を使わず、SQL の `CREATE FUNCTION ... RETURNS trigger
//! LANGUAGE plpgsql` と `CREATE TRIGGER` だけで定義した本体を、公開 API（`execute_sql_in_session`・
//! `execute_sql_in_txn`）から発火させる。注入 invoker 側（`core::trigger_ledger_tests` ほか）は発火側の
//! 意味論（呼び出し回数・戻り値・abort の注入）の固定用に残し、本ファイルは「本物の本体でも同じ性質が成り立つ」
//! ことを固定する。crash 耐性は `examples/crash_tool_sequence.rs` の `*-trigger`（`scripts/crash_test_sequence.sh`）。
//! 対象表は SCHEMA 配下の `s.tt`（共有 public 表へのトリガは定義時に拒否される。Issue #3361）、本体の書き込み先は
//! public 表の裸の名前に限られる（既知の差。`docs/design/plpgsql-subset.md`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::trigger_limits::TriggerLimits;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_at(path: &std::path::Path, mode: LedgerMode) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode)
}

fn open(label: &str, mode: LedgerMode) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path, mode);
    (guard, core)
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), &mut session(), sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e,
    }
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    err(core, tenant, sql).wire_code().to_string()
}

fn ints(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    let SqlOutcome::Query(q) = ok(core, tenant, sql) else {
        panic!("expected query for {sql}");
    };
    let mut v: Vec<i64> = q
        .rows
        .into_iter()
        .filter_map(|r| r.cells.into_iter().next())
        .map(|c| match c {
            Cell::SignedInteger(v) => v,
            Cell::Integer(v) => i64::try_from(v).expect("fits"),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

/// 対象表 `s.tt`・監査表 `audit`・カウンタ表 `ctr`（種行 1 件）。
fn tables(core: &EngineCore, tenant: &str) {
    ok(core, tenant, "CREATE SCHEMA s");
    ok(core, tenant, "CREATE TABLE s.tt (n INTEGER)");
    // public 表は名前空間を共有し、行だけがテナントごとに分かれる。2 テナント目以降は作成済みを許す。
    for ddl in [
        "CREATE TABLE audit (n INTEGER)",
        "CREATE TABLE ctr (c INTEGER)",
    ] {
        if let Err(e) = run(core, tenant, ddl) {
            assert_eq!(e.wire_code(), "42P07", "{ddl}: {e}");
        }
    }
    ok(core, tenant, "INSERT INTO ctr (c) VALUES (0)");
}

fn func(core: &EngineCore, tenant: &str, name: &str, body: &str) {
    ok(
        core,
        tenant,
        &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
    );
}

fn trigger(core: &EngineCore, tenant: &str, name: &str, when_event: &str, level: &str, f: &str) {
    ok(
        core,
        tenant,
        &format!(
            "CREATE TRIGGER {name} {when_event} ON s.tt FOR EACH {level} EXECUTE FUNCTION {f}()"
        ),
    );
}

/// BEFORE ROW（NEW 書き換え）・AFTER ROW（本体 INSERT）・AFTER STATEMENT（本体 UPDATE）を全部載せる。
fn full_triggers(core: &EngineCore, tenant: &str) {
    func(
        core,
        tenant,
        "bump",
        "BEGIN NEW.n := NEW.n + 100; RETURN NEW; END",
    );
    func(
        core,
        tenant,
        "log_row",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NULL; END",
    );
    func(
        core,
        tenant,
        "count_stmt",
        "BEGIN UPDATE ctr SET c = c + 1 WHERE c >= 0; RETURN NULL; END",
    );
    trigger(core, tenant, "tb", "BEFORE INSERT", "ROW", "bump");
    trigger(core, tenant, "ta", "AFTER INSERT", "ROW", "log_row");
    trigger(
        core,
        tenant,
        "ts",
        "AFTER INSERT",
        "STATEMENT",
        "count_stmt",
    );
}

fn full_setup(core: &EngineCore, tenant: &str) {
    tables(core, tenant);
    full_triggers(core, tenant);
}

// ---- A. RLS 越境 ----

// 3 テナントが同名の表・関数・トリガを持つ。t1 の本体（INSERT・UPDATE）は t2・t3 の行へ届かない。
#[test]
fn body_writes_never_reach_other_tenants() {
    let (_g, core) = open("t45prod-cross", LedgerMode::Optional);
    for t in ["t1", "t2", "t3"] {
        tables(&core, t);
        ok(&core, t, "INSERT INTO audit (n) VALUES (7)");
    }
    full_triggers(&core, "t1");
    // t2 は同名のトリガを持たない。t2 の書き込みで t1 のトリガは発火しない。
    ok(&core, "t2", "INSERT INTO s.tt (n) VALUES (1)");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (2)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![7, 101, 102]);
    assert_eq!(ints(&core, "t1", "SELECT c FROM ctr"), vec![1]);
    for t in ["t2", "t3"] {
        assert_eq!(ints(&core, t, "SELECT n FROM audit"), vec![7]);
        assert_eq!(ints(&core, t, "SELECT c FROM ctr"), vec![0]);
    }
    assert_eq!(ints(&core, "t2", "SELECT n FROM s.tt"), vec![1]);
    assert!(ints(&core, "t3", "SELECT n FROM s.tt").is_empty());
}

// 本体が存在しない表を参照して失敗するときのエラー（`wire_code`・文言）は、他テナントが同名の表を持つ前後で同一。
#[test]
fn body_error_does_not_reveal_other_tenants() {
    let (_g, core) = open("t45prod-oracle", LedgerMode::Optional);
    ok(&core, "a", "CREATE SCHEMA s");
    ok(&core, "a", "CREATE TABLE s.tt (n INTEGER)");
    func(
        &core,
        "a",
        "ghost",
        "BEGIN INSERT INTO ghost_tbl (n) VALUES (NEW.n); RETURN NEW; END",
    );
    trigger(&core, "a", "tb", "BEFORE INSERT", "ROW", "ghost");
    let before = err(&core, "a", "INSERT INTO s.tt (n) VALUES (1)");
    // 他テナント c が SCHEMA 配下に同名の表を作っても、a の失敗は変わらず、c の行にも届かない。
    ok(&core, "c", "CREATE SCHEMA s");
    ok(&core, "c", "CREATE TABLE s.ghost_tbl (n INTEGER)");
    ok(&core, "c", "INSERT INTO s.ghost_tbl (n) VALUES (1)");
    let after = err(&core, "a", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(before.wire_code(), after.wire_code());
    assert_eq!(before.to_string(), after.to_string());
    assert!(ints(&core, "a", "SELECT n FROM s.tt").is_empty());
    assert_eq!(ints(&core, "c", "SELECT n FROM s.ghost_tbl"), vec![1]);
}

// 共有 public 表への CREATE TRIGGER は定義時に 0A000（Issue #3361 の回帰）。
#[test]
fn trigger_on_shared_public_table_is_rejected() {
    let (_g, core) = open("t45prod-public", LedgerMode::Optional);
    ok(&core, "t1", "CREATE TABLE pub (n INTEGER)");
    func(
        &core,
        "t1",
        "bump",
        "BEGIN NEW.n := NEW.n + 1; RETURN NEW; END",
    );
    assert_eq!(
        code(
            &core,
            "t1",
            "CREATE TRIGGER tb BEFORE INSERT ON pub FOR EACH ROW EXECUTE FUNCTION bump()"
        ),
        "0A000"
    );
}

// ---- B. 台帳の整合 ----

// 再送は 23505 でトリガ（BEFORE・AFTER ROW・AFTER STATEMENT の本体 INSERT・UPDATE）を再実行せず、
// 内容違いの再送は 22023。BEFORE の書き換え前の値で照合する。
#[test]
fn replay_does_not_refire_any_trigger_level() {
    let (_g, core) = open("t45prod-replay", LedgerMode::Optional);
    full_setup(&core, "t1");
    let sql = "INSERT INTO s.tt (n) VALUES (1), (2) USING OPERATION_ID 'op-1'";
    ok(&core, "t1", sql);
    // 正の対照: 本番の本体が実際に発火している。
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![101, 102]);
    assert_eq!(ints(&core, "t1", "SELECT c FROM ctr"), vec![1]);
    for _ in 0..2 {
        assert_eq!(code(&core, "t1", sql), "23505");
    }
    // 末尾 `;` 付きの単独文でも同じ。
    assert_eq!(code(&core, "t1", &format!("{sql};")), "23505");
    assert_eq!(
        code(
            &core,
            "t1",
            "INSERT INTO s.tt (n) VALUES (1), (3) USING OPERATION_ID 'op-1'"
        ),
        "22023"
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![101, 102]);
    assert_eq!(ints(&core, "t1", "SELECT c FROM ctr"), vec![1]);
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![101, 102]);
}

// Ledgered（必須）: 外側の省略はトリガを呼ぶ前に 23502。本体の書き込みは operation_id を要求されない。
#[test]
fn ledgered_mode_requires_outer_op_id_only() {
    let (_g, core) = open("t45prod-ledgered", LedgerMode::Ledgered);
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.tt (n INTEGER)");
    ok(&core, "t1", "CREATE TABLE audit (n INTEGER)");
    ok(&core, "t1", "CREATE TABLE ctr (c INTEGER)");
    ok(
        &core,
        "t1",
        "INSERT INTO ctr (c) VALUES (0) USING OPERATION_ID 'seed'",
    );
    full_triggers(&core, "t1");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
        "23502"
    );
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
    ok(
        &core,
        "t1",
        "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-a'",
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![101]);
    assert_eq!(ints(&core, "t1", "SELECT c FROM ctr"), vec![1]);
}

// CompareOnlyWithoutLedger（台帳を持たない構成）でも本体の書き込みは台帳に触れず成功する。
#[test]
fn compare_only_without_ledger_fires_without_touching_ledger() {
    let (_g, core) = open("t45prod-noledger", LedgerMode::CompareOnlyWithoutLedger);
    full_setup(&core, "t1");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![101]);
    assert_eq!(ints(&core, "t1", "SELECT c FROM ctr"), vec![1]);
}

// 本体の書き込み先表（audit・ctr）には operation_id が載らず、外側の文の再送だけが 23505 で拒否される。
// （テナント SCHEMA 配下の表の台帳キーは内部 ID のため、外側の表への直接照会はしない。）
#[test]
fn body_target_tables_are_not_recorded() {
    use engine::recovery::ledger::LedgerLookup;
    use engine::recovery::required_op_id::OperationId;
    let (_g, core) = open("t45prod-recorded", LedgerMode::Optional);
    full_setup(&core, "t1");
    let sql = "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-r'";
    ok(&core, "t1", sql);
    assert_eq!(code(&core, "t1", sql), "23505");
    let op = OperationId::parse("op-r").expect("op id");
    for table in ["audit", "ctr"] {
        let got = core
            .operation_recorded(&ctx("t1"), table, &op)
            .unwrap_or_else(|e| panic!("lookup {table}: {e}"));
        assert_eq!(got, LedgerLookup::NotRecorded, "{table}");
    }
}

// 本体が途中で失敗した文は行・監査・カウンタ・台帳のどれも残さず、同じ operation_id の再試行が 1 回分だけ発火する。
#[test]
fn failed_body_leaves_nothing_and_retry_fires_once() {
    let (_g, core) = open("t45prod-abort", LedgerMode::Optional);
    tables(&core, "t1");
    func(
        &core,
        "t1",
        "maybe_fail",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); \
         IF NEW.n = 2 THEN RAISE EXCEPTION 'boom'; END IF; RETURN NEW; END",
    );
    trigger(&core, "t1", "tb", "BEFORE INSERT", "ROW", "maybe_fail");
    let bad = "INSERT INTO s.tt (n) VALUES (1), (2) USING OPERATION_ID 'op-f'";
    assert!(run(&core, "t1", bad).is_err());
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
    // 失敗した文は台帳に載らないため、同じ operation_id の正しい文が成功する（23505 にならない）。
    let good = "INSERT INTO s.tt (n) VALUES (1), (3) USING OPERATION_ID 'op-f'";
    ok(&core, "t1", good);
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1, 3]);
    assert_eq!(code(&core, "t1", good), "23505");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1, 3]);
}

// 明示 Tx: COMMIT で本表・監査・カウンタが残り、ROLLBACK で全部残らない。COMMIT 済みの operation_id の再送は 23505。
#[test]
fn explicit_transaction_commit_rollback_and_resend() {
    let (_g, core) = open("t45prod-txn", LedgerMode::Optional);
    full_setup(&core, "t1");
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'tx-1'",
        "COMMIT",
        "BEGIN",
        "INSERT INTO s.tt (n) VALUES (2) USING OPERATION_ID 'tx-2'",
        "ROLLBACK",
        "BEGIN",
    ] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let resend = core.execute_sql_in_txn(
        &c,
        &mut s,
        &mut txn,
        "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'tx-1'",
    );
    assert!(matches!(resend, Err(SqlSurfaceError::DuplicateOperationId)));
    let _ = core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK");
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![101]);
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![101]);
    assert_eq!(ints(&core, "t1", "SELECT c FROM ctr"), vec![1]);
    // ROLLBACK した operation_id は未消費で、そのまま使える。
    ok(
        &core,
        "t1",
        "INSERT INTO s.tt (n) VALUES (2) USING OPERATION_ID 'tx-2'",
    );
    assert_eq!(ints(&core, "t1", "SELECT c FROM ctr"), vec![2]);
}

// 再オープン後の再送も 23505 で、トリガは再実行されない。
#[test]
fn replay_after_reopen_does_not_refire() {
    let path = unique_db_path("t45prod-reopen");
    let _g = CleanupGuard(path.clone());
    let sql = "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-ro'";
    {
        let core = core_at(&path, LedgerMode::Optional);
        full_setup(&core, "t1");
        ok(&core, "t1", sql);
    }
    let core = core_at(&path, LedgerMode::Optional);
    assert_eq!(code(&core, "t1", sql), "23505");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![101]);
    assert_eq!(ints(&core, "t1", "SELECT c FROM ctr"), vec![1]);
}

// ---- C. 受理形・拒否形（fail-closed） ----

// 末尾 `;` 付きの単独文（INSERT・UPDATE・DELETE）で、本体の INSERT が効く。
#[test]
fn semicolon_terminated_statements_fire_all_events() {
    let (_g, core) = open("t45prod-semicolon", LedgerMode::Optional);
    tables(&core, "t1");
    func(
        &core,
        "t1",
        "log_ev",
        "BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit (n) VALUES (OLD.n); \
         ELSE INSERT INTO audit (n) VALUES (NEW.n); END IF; RETURN NULL; END",
    );
    trigger(
        &core,
        "t1",
        "ta",
        "AFTER INSERT OR UPDATE OR DELETE",
        "ROW",
        "log_ev",
    );
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1);");
    ok(&core, "t1", "UPDATE s.tt SET n = 2 WHERE n = 1;");
    ok(&core, "t1", "DELETE FROM s.tt WHERE n = 2;");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1, 2, 2]);
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
}

// 未対応の本体の形は 0A000 で、本表・監査・台帳に残渣を残さず、続く同一 operation_id の正しい文が成功する。
#[test]
fn unsupported_body_forms_are_0a000_without_residue() {
    let (_g, core) = open("t45prod-unsupported", LedgerMode::Optional);
    tables(&core, "t1");
    ok(&core, "t1", "CREATE TABLE other (n INTEGER)");
    let forms = [
        "INSERT INTO audit (n) SELECT 1",
        "INSERT INTO audit (n) VALUES (1) ON CONFLICT DO NOTHING",
        "INSERT INTO audit (n) VALUES (1) RETURNING n",
    ];
    for (i, form) in forms.iter().enumerate() {
        let f = format!("bad{i}");
        func(
            &core,
            "t1",
            &f,
            &format!("BEGIN INSERT INTO other (n) VALUES (9); {form}; RETURN NEW; END"),
        );
        trigger(&core, "t1", &format!("tb{i}"), "BEFORE INSERT", "ROW", &f);
        let sql = "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-u'";
        assert_eq!(code(&core, "t1", sql), "0A000", "form: {form}");
        for t in ["s.tt", "audit", "other"] {
            assert!(ints(&core, "t1", &format!("SELECT n FROM {t}")).is_empty());
        }
        ok(&core, "t1", &format!("DROP TRIGGER tb{i} ON s.tt"));
    }
    // 拒否された文の operation_id は消費されていない。
    ok(
        &core,
        "t1",
        "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-u'",
    );
}

// 深さ上限は本番 invoker では再帰を構成できない（本体の書き込み先は public 表の裸の名前に限られ、
// トリガ表は SCHEMA 配下に限られる）ため、注入 invoker の `core::trigger_depth_tests` が担う。ここでは
// 上限を下げても通常の 1 段の発火が拒否されないことだけを固定する。
#[test]
fn lowered_depth_limit_still_allows_one_level() {
    let (_g, core) = open("t45prod-depth", LedgerMode::Optional);
    let core = core.with_trigger_limits(TriggerLimits::new(1));
    full_setup(&core, "t1");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![101]);
}
