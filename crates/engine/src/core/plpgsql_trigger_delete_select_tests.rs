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

/// 表 `tt (n)`・`audit (n)` を作る。
fn base_tables(core: &EngineCore) {
    ok(core, "t1", "CREATE TABLE tt (n INTEGER)");
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
    trigger(&core, "ta", "AFTER INSERT", "tt", "ROW", "prune");
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (3)");
    assert_eq!(sorted(ints(&core, "t1", "SELECT n FROM audit")), vec![1, 3]);
}

// (2) 子表の手動カスケード（AFTER DELETE ROW で子の行を消す）。
#[test]
fn after_delete_row_cascades_manually() {
    let (core, _g) = core_with("trgdel-cascade", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (1), (2)");
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1), (1), (2)");
    create_fn(
        &core,
        "t1",
        "cascade",
        "BEGIN DELETE FROM audit WHERE n = OLD.n; RETURN OLD; END",
    );
    trigger(&core, "tc", "AFTER DELETE", "tt", "ROW", "cascade");
    ok(&core, "t1", "DELETE FROM tt WHERE n = 1");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![2]);
    assert_eq!(ints(&core, "t1", "SELECT n FROM tt"), vec![2]);
}

// (3) 本体の SELECT が自 txn の未 commit の変更を見る（BEFORE ROW が先の行の書き込みを数える）。
#[test]
fn body_select_sees_rows_written_by_the_same_statement() {
    let (core, _g) = core_with("trgsel-self", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "number_rows",
        "DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM tt; NEW.n := c; RETURN NEW; END",
    );
    trigger(&core, "tb", "BEFORE INSERT", "tt", "ROW", "number_rows");
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (10), (10), (10)");
    assert_eq!(sorted(ints(&core, "t1", "SELECT n FROM tt")), vec![0, 1, 2]);
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
        "DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM audit; INSERT INTO tt (n) VALUES (c); RETURN NEW; END",
    );
    // `tt` ではなく別表 `src` の INSERT で発火させる（自己再帰を避ける）。
    ok(&core, "t1", "CREATE TABLE src (n INTEGER)");
    trigger(&core, "ts", "AFTER INSERT", "src", "ROW", "log_count");
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "INSERT INTO audit (n) VALUES (1), (2)",
        "INSERT INTO src (n) VALUES (9)",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    assert_eq!(ints(&core, "t1", "SELECT n FROM tt"), vec![2]);
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
    trigger(&core, "tb", "BEFORE INSERT", "tt", "ROW", "sum_audit");
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (0)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM tt"), vec![6]);
}

// (4) 発火表そのものを読む・消す（表ハンドルの再入確認）。BEFORE ROW・AFTER ROW・AFTER STATEMENT で読み、
// ループ後の AFTER ROW・AFTER STATEMENT では同じ表を消せる。
#[test]
fn firing_table_can_be_read_everywhere_and_deleted_after_the_loop() {
    let (core, _g) = core_with("trgdel-self", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (1), (2), (3)");
    create_fn(
        &core,
        "t1",
        "read_self",
        "DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM tt; INSERT INTO audit (n) VALUES (c); RETURN NEW; END",
    );
    trigger(&core, "tb", "BEFORE INSERT", "tt", "ROW", "read_self");
    trigger(&core, "ta", "AFTER INSERT", "tt", "ROW", "read_self");
    trigger(&core, "ts", "AFTER INSERT", "tt", "STATEMENT", "read_self");
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (4)");
    // BEFORE ROW は 3 行・AFTER ROW と AFTER STATEMENT は 4 行を見る。
    assert_eq!(
        sorted(ints(&core, "t1", "SELECT n FROM audit")),
        vec![3, 4, 4]
    );

    ok(&core, "t1", "DROP TRIGGER tb ON tt");
    ok(&core, "t1", "DROP TRIGGER ta ON tt");
    ok(&core, "t1", "DROP TRIGGER ts ON tt");
    create_fn(
        &core,
        "t1",
        "prune_self",
        "BEGIN DELETE FROM tt WHERE n < 3; RETURN NEW; END",
    );
    trigger(&core, "tp", "AFTER INSERT", "tt", "ROW", "prune_self");
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (5)");
    assert_eq!(sorted(ints(&core, "t1", "SELECT n FROM tt")), vec![3, 4, 5]);
    create_fn(
        &core,
        "t1",
        "prune_stmt",
        "BEGIN DELETE FROM tt WHERE n = 5; RETURN NULL; END",
    );
    trigger(&core, "tq", "AFTER INSERT", "tt", "STATEMENT", "prune_stmt");
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (6)");
    assert_eq!(sorted(ints(&core, "t1", "SELECT n FROM tt")), vec![3, 4, 6]);
}

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
    trigger(&core, "ta", "AFTER INSERT", "tt", "ROW", "swap");
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (1);");
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
    trigger(&core, "td", "AFTER DELETE", "tt", "ROW", "on_del");
    ok(&core, "t1", "DELETE FROM tt WHERE n = 1;");
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
    trigger(&core, "tb", "BEFORE INSERT", "tt", "ROW", "wipe");
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (0)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM tt"), vec![2]);
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
    assert_eq!(ints(&core, "t2", "SELECT n FROM audit"), vec![7, 7]);
    // 他テナントの関数は t2 から見えない（トリガも持たない）ので、t2 の挿入は何も消さない。
    ok(&core, "t2", "INSERT INTO tt (n) VALUES (0)");
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
    admin_run("CREATE TABLE tt (n INTEGER)");
    admin_run("CREATE TABLE audit (n INTEGER)");
    admin_run("INSERT INTO audit (n) VALUES (1)");
    admin_run(
        "CREATE FUNCTION prune() RETURNS trigger LANGUAGE plpgsql AS $$ \
         DECLARE c INTEGER; BEGIN SELECT count(*) INTO c FROM audit; DELETE FROM audit; RETURN NEW; END $$",
    );
    admin_run("CREATE TRIGGER tb BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION prune()");
    admin_run("GRANT INSERT ON tt TO alice");
    assert_eq!(alice_code("INSERT INTO tt (n) VALUES (1)"), "42501");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    admin_run("GRANT SELECT ON audit TO alice");
    assert_eq!(alice_code("INSERT INTO tt (n) VALUES (1)"), "42501");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    admin_run("GRANT DELETE ON audit TO alice");
    assert_eq!(alice_code("INSERT INTO tt (n) VALUES (1)"), "ok");
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
}

// (9) 深さ上限: AFTER STATEMENT DELETE トリガの本体が同じ表を `DELETE` すると 0 行でも再発火し、`54001`。
#[test]
fn recursive_body_delete_hits_the_depth_limit() {
    let (core, _g) = core_with("trgdel-depth", LedgerMode::Optional);
    ok(&core, "t1", "CREATE TABLE r (n INTEGER)");
    ok(&core, "t1", "INSERT INTO r (n) VALUES (1)");
    create_fn(
        &core,
        "t1",
        "again",
        "BEGIN DELETE FROM r; RETURN NULL; END",
    );
    trigger(&core, "tr", "AFTER DELETE", "r", "STATEMENT", "again");
    assert_eq!(code(&core, "t1", "DELETE FROM r"), "54001");
    assert_eq!(ints(&core, "t1", "SELECT n FROM r"), vec![1]);
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
        "INSERT INTO tt (n) VALUES (1) USING OPERATION_ID 'seed-c'",
    );
    create_fn(
        &core,
        "t1",
        "prune",
        "BEGIN DELETE FROM audit WHERE n = OLD.n; DELETE FROM audit WHERE n = 2; RETURN OLD; END",
    );
    trigger(&core, "td", "AFTER DELETE", "tt", "ROW", "prune");
    let sql = "DELETE FROM tt WHERE n = 1 USING OPERATION_ID 'op-del'";
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
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    for (i, stmt) in [
        "DELETE FROM audit RETURNING n",
        "DELETE FROM audit USING tt WHERE audit.n = tt.n",
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
        ok(&core, "t1", "DROP TRIGGER IF EXISTS tb ON tt");
        trigger(&core, "tb", "BEFORE INSERT", "tt", "ROW", &f);
        let c = code(&core, "t1", "INSERT INTO tt (n) VALUES (1)");
        assert!(c == "0A000" || c == "42601", "{stmt}: unexpected code {c}");
        assert!(ints(&core, "t1", "SELECT n FROM tt").is_empty(), "{stmt}");
        assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1], "{stmt}");
    }
}

// (12) 自己変更のガード: 外側の行ループが進行中の表への本体の `DELETE` は `0A000` で、外側の文全体が取り消される。
//
// intentional: docs/design/trigger-update-delete.md
// PG は `27000` を返す形（行単位の検出）。本実装は表単位の暫定ガードで `0A000`（D6 の既知の差。`ErrorClass` に
// 対応 variant が無い）。
#[test]
fn deleting_the_table_being_modified_in_a_row_loop_is_rejected() {
    let (core, _g) = core_with("trgdel-selfmod", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (1), (2)");
    create_fn(
        &core,
        "t1",
        "eat_self",
        "BEGIN DELETE FROM tt WHERE n = 2; RETURN NEW; END",
    );
    // INSERT の BEFORE ROW。
    trigger(&core, "tb", "BEFORE INSERT", "tt", "ROW", "eat_self");
    assert_eq!(
        code(&core, "t1", "INSERT INTO tt (n) VALUES (3), (4)"),
        "0A000"
    );
    assert_eq!(sorted(ints(&core, "t1", "SELECT n FROM tt")), vec![1, 2]);
    ok(&core, "t1", "DROP TRIGGER tb ON tt");
    // DELETE の BEFORE ROW（未処理の候補行を消す形）。
    create_fn(
        &core,
        "t1",
        "eat_other",
        "BEGIN DELETE FROM tt WHERE n = 2; RETURN OLD; END",
    );
    trigger(&core, "tb", "BEFORE DELETE", "tt", "ROW", "eat_other");
    assert_eq!(code(&core, "t1", "DELETE FROM tt WHERE n > 0"), "0A000");
    assert_eq!(sorted(ints(&core, "t1", "SELECT n FROM tt")), vec![1, 2]);
    // 別表を経由して元の表へ戻る連鎖も拒否される（BEFORE ROW の本体が別表を消し、その AFTER DELETE が元の表を消す）。
    ok(&core, "t1", "DROP TRIGGER tb ON tt");
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    create_fn(
        &core,
        "t1",
        "eat_audit",
        "BEGIN DELETE FROM audit; RETURN OLD; END",
    );
    trigger(&core, "tb", "BEFORE DELETE", "tt", "ROW", "eat_audit");
    create_fn(
        &core,
        "t1",
        "eat_tt",
        "BEGIN DELETE FROM tt WHERE n = 2; RETURN OLD; END",
    );
    trigger(&core, "tc", "AFTER DELETE", "audit", "ROW", "eat_tt");
    assert_eq!(code(&core, "t1", "DELETE FROM tt WHERE n = 1"), "0A000");
    assert_eq!(sorted(ints(&core, "t1", "SELECT n FROM tt")), vec![1, 2]);
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
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
        "tt",
        "ROW",
        "catch_after_delete",
    );
    assert_eq!(code(&core, "t1", "INSERT INTO tt (n) VALUES (2)"), "0A000");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    assert!(ints(&core, "t1", "SELECT n FROM tt").is_empty());
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
    trigger(&core, "tb", "BEFORE INSERT", "tt", "ROW", "boom");
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "INSERT INTO tt (n) VALUES (1)")
        .expect_err("trigger body raises");
    assert_eq!(e.wire_code(), "P0001");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT 1")
        .expect_err("failed transaction");
    assert_eq!(e.wire_code(), "25P02");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    assert!(ints(&core, "t1", "SELECT n FROM tt").is_empty());
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
    trigger(&core, "tb", "BEFORE INSERT", "tt", "ROW", "clash");
    assert_eq!(code(&core, "t1", "INSERT INTO tt (n) VALUES (1)"), "42702");
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
    create_fn(
        &core,
        "t1",
        "sub",
        "BEGIN DELETE FROM audit WHERE n IN (SELECT n FROM tt); RETURN NEW; END",
    );
    trigger(&core, "ta", "AFTER INSERT", "tt", "ROW", "sub");
    ok(
        &core,
        "t1",
        "INSERT INTO tt (n) VALUES (2) USING OPERATION_ID 'o1'",
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
    trigger(&core, "tb", "BEFORE INSERT", "tt", "ROW", "via_view");
    ok(&core, "t1", "INSERT INTO tt (n) VALUES (0)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM tt"), vec![2]);
}

// 行ループ進行中の表へ FK 参照アクション（ON DELETE CASCADE）で波及する本体の `DELETE` も拒否される（D6 の暫定ガード）。
// 子表への外側 INSERT の BEFORE ROW が親表を消すと、子の行が連鎖で消えるため `0A000`。
//
// intentional: docs/design/trigger-update-delete.md
#[test]
fn delete_cascading_into_the_table_in_a_row_loop_is_rejected() {
    let (core, _g) = core_with("trgdel-fkcascade", LedgerMode::Optional);
    ok(&core, "t1", "CREATE TABLE parent (k BIGINT PRIMARY KEY)");
    ok(
        &core,
        "t1",
        "CREATE TABLE child (n BIGINT, pid BIGINT REFERENCES parent ON DELETE CASCADE)",
    );
    ok(&core, "t1", "INSERT INTO parent (k) VALUES (1), (2)");
    ok(&core, "t1", "INSERT INTO child (n, pid) VALUES (10, 1)");
    create_fn(
        &core,
        "t1",
        "eat_parent",
        "BEGIN DELETE FROM parent WHERE k = 1; RETURN NEW; END",
    );
    trigger(&core, "tb", "BEFORE INSERT", "child", "ROW", "eat_parent");
    assert_eq!(
        code(&core, "t1", "INSERT INTO child (n, pid) VALUES (11, 2)"),
        "0A000"
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM child"), vec![10]);
    assert_eq!(ints(&core, "t1", "SELECT k FROM parent").len(), 2);
}
