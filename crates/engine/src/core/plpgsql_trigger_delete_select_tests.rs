//! トリガ本体の `DELETE` と表を読む `SELECT` の結合テスト（TABLE-45・SQL-68・RLS-10。Issue #3187）。
//!
//! 本番の `PlpgsqlTriggerInvoker` を `with_trigger_invoker_for_test` で注入し、SQL の
//! `CREATE FUNCTION ... RETURNS trigger LANGUAGE plpgsql` と `CREATE TRIGGER`・`INSERT`／`DELETE` だけで
//! 発火を通す。窓口（`TriggerWriteHandle`）の `run_delete`・`with_read_source` と、本体の文種ゲート
//! （`core::plpgsql_trigger_host`）、外側の行ループ中の同表 `DELETE` を拒否する暫定ガード
//! （`trigger::row_loop`）の振る舞いを固定する。期待値は PostgreSQL 17 の挙動。既知の差は
//! `docs/design/plpgsql-subset.md` の「Issue #3187 追記」と `docs/design/trigger-update-delete.md`。
//! 本体 `UPDATE` は #3186、wire 経由の E2E は #3185、RLS・台帳・crash の網羅的な再検証は #3188 の担当。

use std::sync::Arc;

use super::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use crate::trigger::call::PlpgsqlTriggerInvoker;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_with(label: &str, mode: LedgerMode) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(mode)
        .with_trigger_invoker_for_test(Arc::new(PlpgsqlTriggerInvoker));
    (core, CleanupGuard(path))
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = session();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(_) => "ok".to_string(),
        Err(e) => e.wire_code().to_string(),
    }
}

fn ints(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    match run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Query(q) => q
            .rows
            .into_iter()
            .map(|r| match &r.cells[0] {
                Cell::SignedInteger(v) => *v,
                Cell::Integer(v) => i64::try_from(*v).expect("fits"),
                other => panic!("unexpected {other:?}"),
            })
            .collect(),
        other => panic!("expected query, got {other:?}"),
    }
}

fn sorted(mut v: Vec<i64>) -> Vec<i64> {
    v.sort_unstable();
    v
}

fn create_fn(core: &EngineCore, tenant: &str, name: &str, body: &str) {
    ok(
        core,
        tenant,
        &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
    );
}

fn trigger(core: &EngineCore, trg: &str, spec: &str, table: &str, level: &str, f: &str) {
    ok(
        core,
        "t1",
        &format!("CREATE TRIGGER {trg} {spec} ON {table} FOR EACH {level} EXECUTE FUNCTION {f}()"),
    );
}

/// テナントの SCHEMA `s` の表 `s.tt (n)`（トリガの対象）と、public の `audit (n)` を作る。共有 public 表のトリガは
/// 定義時に拒否される（Issue #3361）。本体の書き込み・読み取り先は public の裸の名前に限られる。
fn base_tables(core: &EngineCore) {
    ok(core, "t1", "CREATE SCHEMA s");
    ok(core, "t1", "CREATE TABLE s.tt (n INTEGER)");
    ok(core, "t1", "CREATE TABLE audit (n INTEGER)");
}

// (1) AFTER INSERT ROW の本体の述語形 `DELETE` が反映される。
#[test]
fn body_predicate_delete_applies() {
    let (core, _g) = core_with("trgdel-pred", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1), (2), (3)");
    create_fn(
        &core,
        "t1",
        "prune",
        "BEGIN DELETE FROM audit WHERE n = NEW.n - 1; RETURN NEW; END",
    );
    trigger(&core, "ta", "AFTER INSERT", "s.tt", "ROW", "prune");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (3)");
    assert_eq!(sorted(ints(&core, "t1", "SELECT n FROM audit")), vec![1, 3]);
}

// (2) 子表の手動カスケード（AFTER DELETE ROW で子の行を消す）。
#[test]
fn after_delete_row_cascades_manually() {
    let (core, _g) = core_with("trgdel-cascade", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (2)");
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1), (1), (2)");
    create_fn(
        &core,
        "t1",
        "cascade",
        "BEGIN DELETE FROM audit WHERE n = OLD.n; RETURN OLD; END",
    );
    trigger(&core, "tc", "AFTER DELETE", "s.tt", "ROW", "cascade");
    ok(&core, "t1", "DELETE FROM s.tt WHERE n = 1");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![2]);
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![2]);
}

// (3) 本体の SELECT が自 txn の未 commit の変更を見る（BEFORE ROW が、先の行で本体が書いた監査行を数える）。
#[test]
fn body_select_sees_rows_written_by_the_same_statement() {
    let (core, _g) = core_with("trgsel-self", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "number_rows",
        "DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM audit; INSERT INTO audit (n) VALUES (c); NEW.n := c; RETURN NEW; END",
    );
    trigger(&core, "tb", "BEFORE INSERT", "s.tt", "ROW", "number_rows");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (10), (10), (10)");
    assert_eq!(
        sorted(ints(&core, "t1", "SELECT n FROM s.tt")),
        vec![0, 1, 2]
    );
    assert_eq!(
        sorted(ints(&core, "t1", "SELECT n FROM audit")),
        vec![0, 1, 2]
    );
}

// (3) 明示 Tx の中で、先の文の未 commit の行が本体の SELECT から見える。
#[test]
fn body_select_sees_uncommitted_rows_of_the_explicit_transaction() {
    let (core, _g) = core_with("trgsel-txn", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "log_count",
        "DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM audit; INSERT INTO dst (n) VALUES (c); RETURN NEW; END",
    );
    ok(&core, "t1", "CREATE TABLE dst (n INTEGER)");
    ok(&core, "t1", "CREATE TABLE s.src (n INTEGER)");
    trigger(&core, "ts", "AFTER INSERT", "s.src", "ROW", "log_count");
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "INSERT INTO audit (n) VALUES (1), (2)",
        "INSERT INTO s.src (n) VALUES (9)",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    assert_eq!(ints(&core, "t1", "SELECT n FROM dst"), vec![2]);
}

// (3) `PERFORM`・`FOR r IN SELECT ... LOOP` も通る。
#[test]
fn perform_and_for_select_loop_work() {
    let (core, _g) = core_with("trgsel-loop", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1), (2), (3)");
    create_fn(
        &core,
        "t1",
        "sum_audit",
        "DECLARE r RECORD; total INTEGER := 0; BEGIN PERFORM n FROM audit; \
         FOR r IN SELECT n FROM audit LOOP total := total + r.n; END LOOP; NEW.n := total; RETURN NEW; END",
    );
    trigger(&core, "tb", "BEFORE INSERT", "s.tt", "ROW", "sum_audit");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (0)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![6]);
}

// 注記（Issue #3361）: 本体の `SELECT`・`DELETE` の対象は共有 `public` 表の裸の名前に限られる（SCHEMA 修飾は窓口の
// 許可リスト外）一方、共有 `public` 表のトリガは定義時に拒否される。そのため「トリガ表と本体の読み書き先が同じ表」
// になる形（発火表そのものの読み取り・ループ後の削除、自己・相互再帰の深さ上限 `54001`、行ループ中の同表 `DELETE` の
// D6 安全弁）は SQL から構成できない。D6 の判定は `tenant/trigger_row_loop.rs`、深さ上限は注入 invoker の
// `core/trigger_depth_tests.rs` が担う。本体の書き込み先に SCHEMA 修飾を許す拡張が入った時点で、本体 SQL 経由の
// 検証をここへ戻す（スコープ外）。

// (5) 外側の文を末尾 `;` 付きで送っても、本体の DELETE・SELECT が効く。
#[test]
fn trailing_semicolon_outer_statements_fire_body_delete_and_select() {
    let (core, _g) = core_with("trgdel-semicolon", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1), (2)");
    create_fn(
        &core,
        "t1",
        "swap",
        "DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM audit; DELETE FROM audit WHERE n = NEW.n; \
         INSERT INTO audit (n) VALUES (c * 10); RETURN NEW; END",
    );
    trigger(&core, "ta", "AFTER INSERT", "s.tt", "ROW", "swap");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1);");
    assert_eq!(
        sorted(ints(&core, "t1", "SELECT n FROM audit")),
        vec![2, 20]
    );
    create_fn(
        &core,
        "t1",
        "on_del",
        "BEGIN DELETE FROM audit WHERE n = 2; RETURN OLD; END",
    );
    trigger(&core, "td", "AFTER DELETE", "s.tt", "ROW", "on_del");
    ok(&core, "t1", "DELETE FROM s.tt WHERE n = 1;");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![20]);
}

// (6) テナント境界: 同じ値の行があっても、本体の SELECT・DELETE は呼び出しテナントの行だけに届く。
#[test]
fn body_select_and_delete_stay_inside_the_tenant() {
    let (core, _g) = core_with("trgdel-tenant", LedgerMode::Optional);
    base_tables(&core);
    for t in ["t1", "t2"] {
        ok(&core, t, "INSERT INTO audit (n) VALUES (7), (7)");
    }
    create_fn(
        &core,
        "t1",
        "wipe",
        "DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM audit WHERE n = 7; NEW.n := c; \
         DELETE FROM audit WHERE n = 7; RETURN NEW; END",
    );
    // t2 にも同じ SCHEMA・表を用意する（トリガは t1 だけが持つ）。
    ok(&core, "t2", "CREATE SCHEMA s");
    ok(&core, "t2", "CREATE TABLE s.tt (n INTEGER)");
    trigger(&core, "tb", "BEFORE INSERT", "s.tt", "ROW", "wipe");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (0)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![2]);
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
    assert_eq!(ints(&core, "t2", "SELECT n FROM audit"), vec![7, 7]);
    // 他テナントの関数は t2 から見えない（トリガも持たない）ので、t2 の挿入は何も消さない。
    ok(&core, "t2", "INSERT INTO s.tt (n) VALUES (0)");
    assert_eq!(ints(&core, "t2", "SELECT n FROM audit"), vec![7, 7]);
}

// (8) 権限: 本体の DELETE・SELECT は発火元のロールで判定され、権限が無ければ `42501`。
#[test]
fn body_delete_and_select_are_checked_against_the_firing_role() {
    use crate::catalog::role::{RoleAttributes, RoleDef};
    let path = unique_db_path("trgdel-priv");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    let mk = |name: &str| {
        storage
            .create_role(
                "t1",
                &RoleDef {
                    id: 0,
                    name: name.to_string(),
                    attributes: RoleAttributes::default(),
                    connection_limit: -1,
                    valid_until: None,
                    scram_verifier: Vec::new(),
                    legacy_phc: Vec::new(),
                },
            )
            .expect("role")
    };
    let admin = mk("admin_r");
    let alice = mk("alice");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_trigger_invoker_for_test(Arc::new(PlpgsqlTriggerInvoker));
    let c = ctx("t1");
    let bound = |role: u64, ddl: bool| {
        let mut s = SessionState::default();
        s.bind_role_ids(role, role);
        assert!(s.bind_resolution_tenant("t1"));
        if ddl {
            s.allow_ddl();
        }
        s
    };
    let admin_run = |sql: &str| {
        let mut s = bound(admin, true);
        core.execute_sql_in_session(&c, &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    };
    let alice_code = |sql: &str| {
        let mut s = bound(alice, false);
        match core.execute_sql_in_session(&c, &mut s, sql) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.wire_code().to_string(),
        }
    };
    admin_run("CREATE SCHEMA s");
    admin_run("CREATE TABLE s.tt (n INTEGER)");
    admin_run("CREATE TABLE audit (n INTEGER)");
    admin_run("INSERT INTO audit (n) VALUES (1)");
    admin_run(
        "CREATE FUNCTION prune() RETURNS trigger LANGUAGE plpgsql AS $$ \
         DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM audit; DELETE FROM audit; RETURN NEW; END $$",
    );
    admin_run("CREATE TRIGGER tb BEFORE INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION prune()");
    admin_run("GRANT USAGE ON SCHEMA s TO alice");
    admin_run("GRANT INSERT ON s.tt TO alice");
    assert_eq!(alice_code("INSERT INTO s.tt (n) VALUES (1)"), "42501");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    admin_run("GRANT SELECT ON audit TO alice");
    assert_eq!(alice_code("INSERT INTO s.tt (n) VALUES (1)"), "42501");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    admin_run("GRANT DELETE ON audit TO alice");
    assert_eq!(alice_code("INSERT INTO s.tt (n) VALUES (1)"), "ok");
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
}

// (10) 台帳: 台帳必須の構成でも、外側の `USING OPERATION_ID` が発火させた本体の DELETE は `23502` にならず、
// 台帳には外側の 1 エントリだけが載る（再送は本体を再実行しない）。
#[test]
fn ledgered_core_body_delete_records_only_the_outer_statement() {
    let (core, _g) = core_with("trgdel-ledger", LedgerMode::Ledgered);
    base_tables(&core);
    ok(
        &core,
        "t1",
        "INSERT INTO audit (n) VALUES (1) USING OPERATION_ID 'seed-a'",
    );
    ok(
        &core,
        "t1",
        "INSERT INTO audit (n) VALUES (2) USING OPERATION_ID 'seed-b'",
    );
    ok(
        &core,
        "t1",
        "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'seed-c'",
    );
    create_fn(
        &core,
        "t1",
        "prune",
        "BEGIN DELETE FROM audit WHERE n = OLD.n; DELETE FROM audit WHERE n = 2; RETURN OLD; END",
    );
    trigger(&core, "td", "AFTER DELETE", "s.tt", "ROW", "prune");
    let sql = "DELETE FROM s.tt WHERE n = 1 USING OPERATION_ID 'op-del'";
    ok(&core, "t1", sql);
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
    ok(
        &core,
        "t1",
        "INSERT INTO audit (n) VALUES (2) USING OPERATION_ID 'seed-d'",
    );
    assert_eq!(code(&core, "t1", sql), "23505");
    assert_eq!(
        ints(&core, "t1", "SELECT n FROM audit"),
        vec![2],
        "resend must not rerun the body"
    );
}

// (11) 窓口が受けない形・許可リスト外の形は固定の `0A000` で拒否され、副作用が残らない。
//
// intentional: docs/design/plpgsql-subset.md
// `DELETE ... RETURNING`・`DELETE ... USING`・データ変更 CTE・ビューへの `DELETE` は、窓口と文種ゲートの
// 許可リストに無い形（設計記録「Issue #3187 追記」）。`FOR UPDATE`・`USING PLAN` 付きの `SELECT` は
// `CREATE FUNCTION` の時点で拒否される（さらに手前の層）。
#[test]
fn forms_outside_the_allowlist_are_rejected_without_side_effects() {
    let (core, _g) = core_with("trgdel-reject", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "CREATE VIEW av AS SELECT n FROM audit");
    ok(&core, "t1", "CREATE TABLE src (n INTEGER)");
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    for (i, stmt) in [
        "DELETE FROM audit RETURNING n",
        "DELETE FROM audit USING src WHERE audit.n = src.n",
        "WITH d AS (DELETE FROM audit RETURNING n) SELECT n FROM d",
        "DELETE FROM av",
    ]
    .iter()
    .enumerate()
    {
        let f = format!("rej{i}");
        create_fn(
            &core,
            "t1",
            &f,
            &format!("DECLARE x INTEGER; BEGIN INSERT INTO audit (n) VALUES (50); {stmt}; RETURN NEW; END"),
        );
        ok(&core, "t1", "DROP TRIGGER IF EXISTS tb ON s.tt");
        trigger(&core, "tb", "BEFORE INSERT", "s.tt", "ROW", &f);
        let c = code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
        assert!(c == "0A000" || c == "42601", "{stmt}: unexpected code {c}");
        assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty(), "{stmt}");
        assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1], "{stmt}");
    }
}

// (13) EXCEPTION 節: ブロック内で DELETE した後の `RAISE` は捕捉されず `0A000`（書き込みを巻き戻せない）。
// DELETE の前の評価エラーは従来どおり捕捉できる。
//
// intentional: docs/design/plpgsql-subset.md
// 書き込み後の EXCEPTION 捕捉は、窓口が savepoint を持たないための既知の差（設計記録「Issue #3184 追記」）。
#[test]
fn exception_block_does_not_catch_after_a_body_delete() {
    let (core, _g) = core_with("trgdel-exception", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    create_fn(
        &core,
        "t1",
        "catch_after_delete",
        "BEGIN BEGIN DELETE FROM audit; RAISE EXCEPTION 'x'; \
         EXCEPTION WHEN OTHERS THEN NEW.n := -2; END; RETURN NEW; END",
    );
    trigger(
        &core,
        "tb",
        "BEFORE INSERT",
        "s.tt",
        "ROW",
        "catch_after_delete",
    );
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (2)"),
        "0A000"
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
}

// (14) 明示 Tx で本体のエラーが起きると Tx は `Failed`（`25P02`）になり、ROLLBACK 後に何も残らない。
#[test]
fn body_delete_error_in_explicit_transaction_fails_the_transaction() {
    let (core, _g) = core_with("trgdel-explicit", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    create_fn(
        &core,
        "t1",
        "boom",
        "BEGIN DELETE FROM audit; RAISE EXCEPTION 'boom'; END",
    );
    trigger(&core, "tb", "BEFORE INSERT", "s.tt", "ROW", "boom");
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "INSERT INTO s.tt (n) VALUES (1)")
        .expect_err("trigger body raises");
    assert_eq!(e.wire_code(), "P0001");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT 1")
        .expect_err("failed transaction");
    assert_eq!(e.wire_code(), "25P02");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
}

// (16) `42702`: 変数名と列名が衝突する本体の DELETE は拒否される。
#[test]
fn body_delete_with_variable_column_clash_is_ambiguous() {
    let (core, _g) = core_with("trgdel-clash", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    create_fn(
        &core,
        "t1",
        "clash",
        "DECLARE n INTEGER := 1; BEGIN DELETE FROM audit WHERE n = n; RETURN NEW; END",
    );
    trigger(&core, "tb", "BEFORE INSERT", "s.tt", "ROW", "clash");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
        "42702"
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
}

// 本体の DELETE の WHERE のサブクエリは、同じ write txn（外側が先に書いた行を含む）で解決される。
#[test]
fn body_delete_where_subquery_sees_uncommitted_rows() {
    let (core, _g) = core_with("trgdel-subq", LedgerMode::Ledgered);
    base_tables(&core);
    ok(
        &core,
        "t1",
        "INSERT INTO audit (n) VALUES (1) USING OPERATION_ID 'a1'",
    );
    ok(
        &core,
        "t1",
        "INSERT INTO audit (n) VALUES (2) USING OPERATION_ID 'a2'",
    );
    ok(&core, "t1", "CREATE TABLE src (n INTEGER)");
    create_fn(
        &core,
        "t1",
        "sub",
        "BEGIN INSERT INTO src (n) VALUES (NEW.n); DELETE FROM audit WHERE n IN (SELECT n FROM src); RETURN NEW; END",
    );
    trigger(&core, "ta", "AFTER INSERT", "s.tt", "ROW", "sub");
    ok(
        &core,
        "t1",
        "INSERT INTO s.tt (n) VALUES (2) USING OPERATION_ID 'o1'",
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
}

// 単純なビューの `SELECT` は解析段で基表へ展開され、通常の表と同じ読み取り経路（RLS・権限）を通る。
#[test]
fn body_select_through_a_simple_view_reads_the_base_table() {
    let (core, _g) = core_with("trgsel-view", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "CREATE VIEW av AS SELECT n FROM audit");
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (4), (5)");
    create_fn(
        &core,
        "t1",
        "via_view",
        "DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM av; NEW.n := c; RETURN NEW; END",
    );
    trigger(&core, "tb", "BEFORE INSERT", "s.tt", "ROW", "via_view");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (0)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![2]);
}

// P0（Cursor Bugbot 指摘）: 本体の `DELETE` が参照アクションで波及させた表（孫以降を含む）も書いた表として
// 記録され、連鎖先を親とする遅延 FK が autocommit は文末、明示 Tx は COMMIT で `23503` になる。
#[test]
fn body_delete_cascade_target_is_checked_by_deferred_foreign_keys() {
    let (core, _g) = core_with("trgdel-cascade-deferred", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "CREATE TABLE gp (k BIGINT PRIMARY KEY)");
    ok(
        &core,
        "t1",
        "CREATE TABLE gc (k BIGINT PRIMARY KEY, pid BIGINT REFERENCES gp ON DELETE CASCADE)",
    );
    ok(
        &core,
        "t1",
        "CREATE TABLE gg (cid BIGINT REFERENCES gc DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(&core, "t1", "INSERT INTO gp (k) VALUES (1)");
    ok(&core, "t1", "INSERT INTO gc (k, pid) VALUES (10, 1)");
    ok(&core, "t1", "INSERT INTO gg (cid) VALUES (10)");
    create_fn(
        &core,
        "t1",
        "eat_gp",
        "BEGIN DELETE FROM gp WHERE k = 1; RETURN NEW; END",
    );
    trigger(&core, "ta", "AFTER INSERT", "s.tt", "ROW", "eat_gp");

    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
        "23503"
    );
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert_eq!(ints(&core, "t1", "SELECT k FROM gc"), vec![10]);

    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "INSERT INTO s.tt (n) VALUES (1)")
        .expect("deferred check waits for COMMIT");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "COMMIT")
        .expect_err("deferred FK violation on the cascaded table");
    assert_eq!(e.wire_code(), "23503");
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert_eq!(ints(&core, "t1", "SELECT k FROM gc"), vec![10]);
}

// codex・Bugbot 指摘の回帰: 同じ子表へ SET NULL の FK が先に、CASCADE の FK が後に到達しても、CASCADE 側の孫表
// （遅延 FK の子）が書いた表として記録され、COMMIT 時（autocommit は文末）に `23503` になる。
#[test]
fn set_null_then_cascade_on_same_child_still_records_grandchild() {
    let (core, _g) = core_with("trgdel-setnull-cascade", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "CREATE TABLE xp (k BIGINT PRIMARY KEY)");
    ok(
        &core,
        "t1",
        "CREATE TABLE xc (k BIGINT PRIMARY KEY, a BIGINT REFERENCES xp ON DELETE SET NULL, b BIGINT REFERENCES xp ON DELETE CASCADE)",
    );
    ok(
        &core,
        "t1",
        "CREATE TABLE xg (cid BIGINT REFERENCES xc DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(&core, "t1", "INSERT INTO xp (k) VALUES (1)");
    ok(&core, "t1", "INSERT INTO xc (k, a, b) VALUES (10, 1, 1)");
    ok(&core, "t1", "INSERT INTO xg (cid) VALUES (10)");
    create_fn(
        &core,
        "t1",
        "eat_xp",
        "BEGIN DELETE FROM xp WHERE k = 1; RETURN NEW; END",
    );
    trigger(&core, "ta", "AFTER INSERT", "s.tt", "ROW", "eat_xp");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
        "23503"
    );
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert_eq!(ints(&core, "t1", "SELECT k FROM xc"), vec![10]);
}

// 行ループ中の表の検出（D6 ガード）と書いた表の記録が使う参照アクションの追跡を、窓口へ直接当てて固定する。
// 注記（Issue #3361）: 行ループ中の表はテナント SCHEMA 配下の表で、そこへ波及させる本体の `DELETE` は public 表に
// 限られるが、SCHEMA 配下の表は public 表を参照できない（`SchemaDependencyForbidden`）ため、SQL からは
// 組めない。追跡の関数を直接呼ぶ。`setup` の DDL を実行したあと、`start` の `DELETE` が届く表（小文字・整列済み）を返す。
fn reached_by_delete(core: &EngineCore, setup: &[&str], start: &str) -> Vec<String> {
    for sql in setup {
        ok(core, "t1", sql);
    }
    let pctx = ctx("t1");
    let txn = core.storage.begin_write_txn().expect("txn");
    let handle = crate::trigger::write_handle::TriggerWriteHandle::new(&txn, &pctx, None);
    let seen = std::cell::RefCell::new(Vec::new());
    let blocked = handle
        .delete_reaches_table(start, |t| {
            seen.borrow_mut().push(t.to_ascii_lowercase());
            false
        })
        .expect("catalog readable");
    assert!(!blocked);
    let mut v = seen.into_inner();
    v.sort();
    v
}

// 子行を `CASCADE` で消すと、その先の `ON DELETE CASCADE` の孫表も届く。
#[test]
fn delete_cascade_reaches_grandchild() {
    let (core, _g) = core_with("trgdel-reach-cascade", LedgerMode::Optional);
    let got = reached_by_delete(
        &core,
        &[
            "CREATE TABLE parent (k BIGINT PRIMARY KEY)",
            "CREATE TABLE child (k BIGINT PRIMARY KEY, pid BIGINT REFERENCES parent ON DELETE CASCADE)",
            "CREATE TABLE gchild (n BIGINT, cid BIGINT REFERENCES child ON DELETE CASCADE)",
            // NO ACTION の子は波及しない。
            "CREATE TABLE inert (n BIGINT, pid BIGINT REFERENCES parent)",
        ],
        "parent",
    );
    assert_eq!(got, vec!["child", "gchild"]);
}

// Cursor Bugbot 指摘の回帰: `SET NULL` は子行を更新するだけで削除しないので、更新された列を参照しない孫表の
// `ON DELETE` は発火しない（`sg` は `sc.k` を参照し、更新される列は `sc.pid`）。
#[test]
fn delete_through_set_null_does_not_reach_grandchild() {
    let (core, _g) = core_with("trgdel-reach-setnull", LedgerMode::Optional);
    let got = reached_by_delete(
        &core,
        &[
            "CREATE TABLE sp (k BIGINT PRIMARY KEY)",
            "CREATE TABLE sc (k BIGINT PRIMARY KEY, pid BIGINT REFERENCES sp ON DELETE SET NULL)",
            "CREATE TABLE sg (n BIGINT, cid BIGINT REFERENCES sc ON DELETE CASCADE)",
        ],
        "sp",
    );
    assert_eq!(got, vec!["sc"]);
}

// codex・Bugbot 指摘の回帰: 同じ子表へ SET NULL の FK が先に、CASCADE の FK が後に到達しても、CASCADE 側の孫表が
// 探索される（SET NULL で訪問済みにした表を CASCADE で再探索しない欠陥の再発防止）。
#[test]
fn set_null_then_cascade_on_same_child_still_reaches_grandchild() {
    let (core, _g) = core_with("trgdel-reach-setnull-cascade", LedgerMode::Optional);
    let got = reached_by_delete(
        &core,
        &[
            "CREATE TABLE xp (k BIGINT PRIMARY KEY)",
            "CREATE TABLE xc (k BIGINT PRIMARY KEY, a BIGINT REFERENCES xp ON DELETE SET NULL, b BIGINT REFERENCES xp ON DELETE CASCADE)",
            "CREATE TABLE xg (cid BIGINT REFERENCES xc ON DELETE CASCADE)",
        ],
        "xp",
    );
    assert_eq!(got, vec!["xc", "xg"]);
}

// codex 指摘の回帰: SET NULL・SET DEFAULT で子のキー列が変わると、そのキーを参照する孫の ON UPDATE の参照アクションが
// 波及する。`NO ACTION` の孫は波及しない。
#[test]
fn delete_through_set_default_reaches_on_update_chain() {
    let (core, _g) = core_with("trgdel-reach-onupdate", LedgerMode::Optional);
    let got = reached_by_delete(
        &core,
        &[
            "CREATE TABLE dp (k BIGINT PRIMARY KEY)",
            "CREATE TABLE dc (k BIGINT PRIMARY KEY, pid BIGINT UNIQUE DEFAULT 99 REFERENCES dp ON DELETE SET DEFAULT)",
            "CREATE TABLE dg (n BIGINT UNIQUE, cp BIGINT REFERENCES dc (pid) ON UPDATE CASCADE)",
            // さらにその先（dg.n を参照する表）へは、dg の更新列が cp なので届かないが、cp を参照する表へは届く。
            "CREATE TABLE dgg (m BIGINT REFERENCES dg (n))",
            "CREATE TABLE dnone (cp BIGINT REFERENCES dc (pid))",
        ],
        "dp",
    );
    assert_eq!(got, vec!["dc", "dg"]);
}
