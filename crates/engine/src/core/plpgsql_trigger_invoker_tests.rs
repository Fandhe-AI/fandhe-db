//! 本番のトリガ関数 invoker と、セッション無しのホスト（`core::plpgsql_trigger_host`）の結合テスト
//! （TABLE-45・SQL-68・RLS-10。Issue #3184）。
//!
//! 既定の invoker（本番の `PlpgsqlTriggerInvoker`。Issue #3185 で既定になった）のコアで、SQL の
//! `CREATE FUNCTION ... RETURNS trigger LANGUAGE plpgsql` と `CREATE TRIGGER`・`INSERT` だけで発火を通す。
//! 期待値は PostgreSQL 17 のトリガの挙動
//! （BEFORE ROW の戻り値・NULL による行スキップ・エラーでの文全体の取り消し）。設計記録は
//! `docs/design/plpgsql-subset.md` の「Issue #3184 追記」。

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

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_with(label: &str, mode: LedgerMode) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode);
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

fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Query(q) => q.rows.into_iter().map(|r| r.cells).collect(),
        other => panic!("expected query, got {other:?}"),
    }
}

fn ints(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    rows(core, tenant, sql)
        .into_iter()
        .map(|r| match &r[0] {
            Cell::SignedInteger(v) => *v,
            Cell::Integer(v) => i64::try_from(*v).expect("fits"),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

fn text(c: &Cell) -> String {
    match c {
        Cell::Text(s) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn affected(out: SqlOutcome) -> u64 {
    match out {
        SqlOutcome::Insert(o) => o.rows_affected,
        other => panic!("expected insert, got {other:?}"),
    }
}

/// SCHEMA `s` の表 `s.tt (n)` と、public の `audit (n)` を作る。
fn base_tables(core: &EngineCore) {
    // 共有 public 表へのトリガは定義時に拒否される（Issue #3361）ため、対象表はテナントの SCHEMA `s` に置く。
    // 本体が書く監査表は public のまま（本体の INSERT は SCHEMA 修飾の書き込み先を受けない）。
    ok(core, "t1", "CREATE SCHEMA s");
    ok(core, "t1", "CREATE TABLE s.tt (n INTEGER)");
    ok(core, "t1", "CREATE TABLE audit (n INTEGER)");
}

fn create_fn(core: &EngineCore, tenant: &str, name: &str, body: &str) {
    ok(
        core,
        tenant,
        &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
    );
}

fn create_trigger(core: &EngineCore, tenant: &str, trg: &str, when: &str, table: &str, f: &str) {
    ok(
        core,
        tenant,
        &format!(
            "CREATE TRIGGER {trg} {when} INSERT ON {table} FOR EACH ROW EXECUTE FUNCTION {f}()"
        ),
    );
}

fn audit_count(core: &EngineCore, tenant: &str) -> usize {
    ints(core, tenant, "SELECT n FROM audit").len()
}

// (1) BEFORE ROW が返した NEW が保存値になる。複数行 VALUES の各行に効く。
#[test]
fn before_row_rewrite_applies_to_every_row() {
    let (core, _g) = core_with("trginv-rewrite", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "bump",
        "BEGIN NEW.n := NEW.n + 100; RETURN NEW; END",
    );
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "bump");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (2)");
    let mut v = ints(&core, "t1", "SELECT n FROM s.tt");
    v.sort_unstable();
    assert_eq!(v, vec![101, 102]);
}

// (2) NULL 返却はその行を書かず、影響行数にも含めず、AFTER ROW にも渡さない。
#[test]
fn before_row_null_skips_the_row() {
    let (core, _g) = core_with("trginv-skip", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "skip2",
        "BEGIN IF NEW.n = 2 THEN RETURN NULL; END IF; RETURN NEW; END",
    );
    create_fn(
        &core,
        "t1",
        "log_after",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NULL; END",
    );
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "skip2");
    create_trigger(&core, "t1", "ta", "AFTER", "s.tt", "log_after");
    let out = run(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (2), (3)").expect("insert");
    assert_eq!(affected(out), 2);
    let mut v = ints(&core, "t1", "SELECT n FROM s.tt");
    v.sort_unstable();
    assert_eq!(v, vec![1, 3]);
    let mut a = ints(&core, "t1", "SELECT n FROM audit");
    a.sort_unstable();
    assert_eq!(a, vec![1, 3], "skipped row must not reach AFTER ROW");
}

// INSERT トリガー（BEFORE／AFTER ROW）のある表でも nullable VECTOR の省略・明示 NULL を受理する
// （トリガーの有無で NULL 保存契約が変わらないこと。PR #2963 レビュー指摘。もとは
// `tests/sql_named/table26_nullable_vector_storage.rs`。共有 public 表のトリガが作れなくなった
// Issue #3361 で、本番の invoker を注入できるここへ移した）。
#[test]
fn nullable_vector_accepted_on_trigger_tables() {
    let (core, _g) = core_with("trginv-nullable-vec", LedgerMode::Optional);
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(
        &core,
        "t1",
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, v VECTOR(3), n TEXT)",
    );
    create_fn(&core, "t1", "trg", "BEGIN RETURN NEW; END");
    create_trigger(&core, "t1", "tb", "BEFORE", "s.t", "trg");
    create_trigger(&core, "t1", "ta", "AFTER", "s.t", "trg");
    ok(&core, "t1", "INSERT INTO s.t (id, n) VALUES (1, 'omitted')");
    ok(
        &core,
        "t1",
        "INSERT INTO s.t (id, v, n) VALUES (2, NULL, 'null')",
    );
    let stored = rows(&core, "t1", "SELECT id, v FROM s.t LIMIT 100");
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().all(|r| matches!(r[1], Cell::Null)));
}

// (3) AFTER ROW・AFTER STATEMENT が本体の INSERT で監査表へ書く。TG_* が正しい。
#[test]
fn after_triggers_write_audit_with_tg_variables() {
    let (core, _g) = core_with("trginv-tgvars", LedgerMode::Optional);
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.tt (n INTEGER)");
    ok(
        &core,
        "t1",
        "CREATE TABLE audit2 (n INTEGER, op TEXT, nm TEXT, wh TEXT, lv TEXT, tbl TEXT, sch TEXT, na INTEGER)",
    );
    create_fn(
        &core,
        "t1",
        "row_audit",
        "BEGIN INSERT INTO audit2 (n, op, nm, wh, lv, tbl, sch, na) \
         VALUES (NEW.n, TG_OP, TG_NAME, TG_WHEN, TG_LEVEL, TG_TABLE_NAME, TG_TABLE_SCHEMA, TG_NARGS); \
         RETURN NULL; END",
    );
    create_fn(
        &core,
        "t1",
        "stmt_audit",
        "BEGIN INSERT INTO audit2 (n, op, nm, wh, lv, tbl, sch, na) \
         VALUES (-1, TG_OP, TG_NAME, TG_WHEN, TG_LEVEL, TG_TABLE_NAME, TG_TABLE_SCHEMA, TG_NARGS); \
         RETURN NULL; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER t_row AFTER INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION row_audit('a', 'b')",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER t_stmt AFTER INSERT ON s.tt FOR EACH STATEMENT EXECUTE FUNCTION stmt_audit()",
    );
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (7)");
    let r = rows(
        &core,
        "t1",
        "SELECT n, op, nm, wh, lv, tbl, sch, na FROM audit2",
    );
    assert_eq!(r.len(), 2, "one row trigger and one statement trigger");
    let find = |lv: &str| {
        r.iter()
            .find(|row| text(&row[4]) == lv)
            .unwrap_or_else(|| panic!("no {lv} row"))
    };
    let row = find("ROW");
    assert_eq!(text(&row[1]), "INSERT");
    assert_eq!(text(&row[2]), "t_row");
    assert_eq!(text(&row[3]), "AFTER");
    assert_eq!(text(&row[5]), "tt");
    assert_eq!(text(&row[6]), "s");
    assert!(matches!(&row[7], Cell::SignedInteger(2) | Cell::Integer(2)));
    let stmt = find("STATEMENT");
    assert_eq!(text(&stmt[2]), "t_stmt");
    assert!(matches!(
        &stmt[7],
        Cell::SignedInteger(0) | Cell::Integer(0)
    ));
}

// (4) 本体のエラーで外側の文が同じ SQLSTATE で失敗し、本表・監査表に何も残らない（autocommit）。
#[test]
fn body_errors_abort_the_whole_statement() {
    let (core, _g) = core_with("trginv-abort", LedgerMode::Optional);
    base_tables(&core);
    ok(
        &core,
        "t1",
        "CREATE TABLE strict_audit (n INTEGER NOT NULL)",
    );
    create_fn(
        &core,
        "t1",
        "boom",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RAISE EXCEPTION 'boom'; END",
    );
    create_fn(
        &core,
        "t1",
        "divz",
        "DECLARE z INTEGER; BEGIN INSERT INTO audit (n) VALUES (NEW.n); z := 1 / 0; RETURN NEW; END",
    );
    create_fn(
        &core,
        "t1",
        "nn",
        "BEGIN INSERT INTO strict_audit (n) VALUES (NULL); RETURN NEW; END",
    );
    for (f, expected) in [("boom", "P0001"), ("divz", "22012"), ("nn", "23502")] {
        ok(&core, "t1", "DROP TRIGGER IF EXISTS tb ON s.tt");
        create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", f);
        assert_eq!(
            code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (2)"),
            expected,
            "{f}"
        );
        assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty(), "{f}");
        assert_eq!(audit_count(&core, "t1"), 0, "{f}");
        assert!(
            ints(&core, "t1", "SELECT n FROM strict_audit").is_empty(),
            "{f}"
        );
    }
}

// (4) 明示 Tx でも同じ。失敗後は `25P02`、ROLLBACK で戻り、何も残らない。
#[test]
fn body_error_in_explicit_transaction_fails_the_transaction() {
    let (core, _g) = core_with("trginv-explicit", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "boom",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RAISE EXCEPTION 'boom'; END",
    );
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "boom");
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
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert_eq!(audit_count(&core, "t1"), 0);
}

// (5) 窓口が受けない書き込みの形は、PG とは異なる既知の差として固定の `0A000` で拒否する。
//
// intentional: docs/design/plpgsql-subset.md
// `ON CONFLICT`・`RETURNING`・`INSERT ... SELECT`・SCHEMA 修飾の書き込み先は、窓口（`TriggerWriteHandle`）の
// 許可リストに無い形（設計記録「Issue #3184 追記」）。
#[test]
fn insert_forms_outside_the_write_handle_are_rejected() {
    let (core, _g) = core_with("trginv-forms", LedgerMode::Optional);
    base_tables(&core);
    let forms = [
        "INSERT INTO audit (n) VALUES (1) ON CONFLICT (id) DO NOTHING",
        "INSERT INTO audit (n) VALUES (1) RETURNING n",
        "INSERT INTO audit (n) SELECT 1",
        "INSERT INTO audit (n) SELECT n FROM audit",
    ];
    for (i, stmt) in forms.iter().enumerate() {
        let f = format!("form{i}");
        create_fn(&core, "t1", &f, &format!("BEGIN {stmt}; RETURN NEW; END"));
        ok(&core, "t1", "DROP TRIGGER IF EXISTS tb ON s.tt");
        create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", &f);
        assert_eq!(
            code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
            "0A000",
            "{stmt}"
        );
        assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty(), "{stmt}");
        assert_eq!(audit_count(&core, "t1"), 0, "{stmt}");
    }
}

// (6) テナント境界: t1 のトリガは t2 の挿入で発火せず、t1 の書いた監査行は t2 から見えない。
// 対象表はテナントごとの SCHEMA 配下（共有 public 表のトリガは定義できない。Issue #3361）。
#[test]
fn trigger_and_its_writes_stay_inside_the_tenant() {
    let (core, _g) = core_with("trginv-tenant", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t2", "CREATE SCHEMA s");
    ok(&core, "t2", "CREATE TABLE s.tt (n INTEGER)");
    create_fn(
        &core,
        "t1",
        "log_n",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END",
    );
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "log_n");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(audit_count(&core, "t1"), 1);
    assert_eq!(audit_count(&core, "t2"), 0, "t2 must not see t1 audit rows");
    ok(&core, "t2", "INSERT INTO s.tt (n) VALUES (2)");
    assert_eq!(
        audit_count(&core, "t1"),
        1,
        "t1 trigger must not fire for t2"
    );
    assert_eq!(audit_count(&core, "t2"), 0);
}

// (6) エラー文言が他テナントの有無で変わらない（t2 に同名の関数があっても t1 の応答は同じ）。
#[test]
fn error_text_does_not_depend_on_other_tenants() {
    let (core, _g) = core_with("trginv-errtext", LedgerMode::Optional);
    base_tables(&core);
    create_fn(&core, "t1", "boom", "BEGIN RAISE EXCEPTION 'boom'; END");
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "boom");
    let before = run(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)")
        .unwrap_err()
        .client_message();
    create_fn(&core, "t2", "boom", "BEGIN RETURN NEW; END");
    let after = run(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)")
        .unwrap_err()
        .client_message();
    assert_eq!(before, after);
}

// (9) 台帳: 外側の INSERT の operation_id だけが台帳に載り、本体の書き込みは台帳を要求も記録もしない。
// 再送は `23505` でトリガ本体を再実行しない。台帳必須（`Ledgered`）の構成で確かめる。
#[test]
fn ledgered_core_records_only_the_outer_statement() {
    let (core, _g) = core_with("trginv-ledger", LedgerMode::Ledgered);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "log_n",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END",
    );
    create_trigger(&core, "t1", "ta", "AFTER", "s.tt", "log_n");
    let sql = "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-1'";
    ok(&core, "t1", sql);
    assert_eq!(audit_count(&core, "t1"), 1);
    assert_eq!(code(&core, "t1", sql), "23505");
    assert_eq!(
        audit_count(&core, "t1"),
        1,
        "resend must not rerun the body"
    );
    // operation_id の無い外側の文は、トリガを呼ぶ前に `23502` で拒否される。
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (2)"),
        "23502"
    );
    assert_eq!(audit_count(&core, "t1"), 1);
}

// (10) 入れ子: 本体が書く先は public の監査表で、その表へのトリガは定義時に `0A000` で拒否される
// （Issue #3361。共有 public 表のトリガを作れない）。以前の「監査表のトリガも発火する」「自己再帰は深さ上限
// `54001`」は SQL から作れないため、深さ上限は注入 invoker の `core/trigger_depth_tests.rs` が担う。
//
// intentional: docs/design/trigger-catalog.md
// 共有 public 表への `CREATE TRIGGER` は、後で書き込みだけが失敗する状態を作らないよう定義時に拒否する。
#[test]
fn triggers_on_shared_public_tables_cannot_be_defined_for_nesting() {
    let (core, _g) = core_with("trginv-nested", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "to_audit2",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END",
    );
    for form in [
        "CREATE TRIGGER t2a AFTER INSERT ON audit FOR EACH ROW EXECUTE FUNCTION to_audit2()",
        "CREATE OR REPLACE TRIGGER t2a AFTER INSERT ON audit FOR EACH ROW EXECUTE FUNCTION to_audit2()",
        "CREATE TRIGGER t2a AFTER INSERT ON public.audit FOR EACH ROW EXECUTE FUNCTION to_audit2()",
    ] {
        assert_eq!(code(&core, "t1", form), "0A000", "{form}");
        // カタログには何も残らない。
        assert_eq!(code(&core, "t1", "DROP TRIGGER t2a ON audit"), "42704");
    }
}

// (10) 入れ子の保険: 共有 public 表に既存のカタログ定義が残っていても（導入前の DB を `create_trigger_for_test`
// で模す）、本体の書き込みが起こす入れ子の発火は固定の `0A000` で文全体を取り消す。本体の書き込みも
// 外側の行も残らない（fail-closed。Issue #3361）。
//
// intentional: docs/design/trigger-execution-model.md
// 共有 public 表のトリガは発火させない（他テナントの書き込みでの挙動が定義できないため）。
#[test]
fn legacy_trigger_on_shared_public_table_is_rejected_when_nested() {
    use super::trigger_insert_tests::{add_trigger, trigger_def};
    use crate::catalog::trigger::{TriggerLevel, TriggerTiming};
    let (core, _g) = core_with("trginv-nested-legacy", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "to_audit",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END",
    );
    create_trigger(&core, "t1", "t1a", "AFTER", "s.tt", "to_audit");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (4)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![4]);
    add_trigger(
        &core.storage,
        "t1",
        "audit",
        &trigger_def("legacy", TriggerTiming::After, TriggerLevel::Row),
    );
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (5)"),
        "0A000"
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![4]);
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![4]);
}

// 保険（Issue #3361）: 共有 public 表に既存のカタログ定義が残っていても、本番の invoker は
// BEFORE／AFTER・ROW／STATEMENT のいずれも INSERT・UPDATE・DELETE・TRUNCATE で固定の `0A000` を返し、
// 行・台帳を残さない。明示 Tx は `25P02` を経て ROLLBACK で戻る。
//
// intentional: docs/design/trigger-execution-model.md
// 発火時の拒否は fail-closed のまま維持する（定義時の拒否の導入前に残った定義への保険）。
#[test]
fn legacy_triggers_on_shared_public_tables_never_fire() {
    use super::trigger_insert_tests::{add_trigger, trigger_def};
    use crate::catalog::trigger::{TriggerEvents, TriggerLevel, TriggerTiming};
    let (core, _g) = core_with("trginv-legacy-fire", LedgerMode::Optional);
    ok(&core, "t1", "CREATE TABLE pub (n INTEGER)");
    ok(&core, "t1", "INSERT INTO pub (n) VALUES (1)");
    for (timing, level) in [
        (TriggerTiming::Before, TriggerLevel::Row),
        (TriggerTiming::After, TriggerLevel::Row),
        (TriggerTiming::Before, TriggerLevel::Statement),
        (TriggerTiming::After, TriggerLevel::Statement),
    ] {
        let mut def = trigger_def("legacy", timing, level);
        def.events = TriggerEvents::from_bits(
            TriggerEvents::INSERT | TriggerEvents::UPDATE | TriggerEvents::DELETE,
        )
        .expect("events");
        add_trigger(&core.storage, "t1", "pub", &def);
        for sql in [
            "INSERT INTO pub (n) VALUES (2)",
            "UPDATE pub SET n = 9",
            "DELETE FROM pub",
        ] {
            assert_eq!(code(&core, "t1", sql), "0A000", "{sql}");
        }
        assert_eq!(ints(&core, "t1", "SELECT n FROM pub"), vec![1]);
        assert!(core
            .storage
            .drop_trigger("t1", "pub", "legacy")
            .expect("drop"));
    }
    // 明示 Tx: 失敗後は `25P02`、ROLLBACK で戻る。
    add_trigger(
        &core.storage,
        "t1",
        "pub",
        &trigger_def("legacy", TriggerTiming::After, TriggerLevel::Row),
    );
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "INSERT INTO pub (n) VALUES (3)")
        .expect_err("legacy trigger is rejected");
    assert_eq!(e.wire_code(), "0A000");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT 1")
        .expect_err("failed transaction");
    assert_eq!(e.wire_code(), "25P02");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(ints(&core, "t1", "SELECT n FROM pub"), vec![1]);
}

// (12) STABLE のトリガ関数は書き込めない（揮発性ゲート）。
//
// intentional: docs/design/plpgsql-subset.md
// PG17 と同じく、STABLE／IMMUTABLE 関数のデータ変更は拒否する（設計記録の揮発性ゲート）。
#[test]
fn stable_trigger_function_cannot_write() {
    let (core, _g) = core_with("trginv-stable", LedgerMode::Optional);
    base_tables(&core);
    ok(
        &core,
        "t1",
        "CREATE FUNCTION st() RETURNS trigger LANGUAGE plpgsql STABLE AS $$ \
         BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END $$",
    );
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "st");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
        "0A000"
    );
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert_eq!(audit_count(&core, "t1"), 0);
}

// (13) EXCEPTION 節: 書き込み前の純粋な評価エラーは捕捉できる。書き込みを試みた後は巻き戻せないため
// 捕捉せず `0A000`（書き込みを残したまま続行する fail-open を作らない）。
//
// intentional: docs/design/plpgsql-subset.md
// 書き込み後の EXCEPTION 捕捉は、窓口が savepoint を持たないための既知の差（設計記録「Issue #3184 追記」）。
#[test]
fn exception_block_catches_only_when_nothing_was_written() {
    let (core, _g) = core_with("trginv-exception", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "catch_eval",
        "DECLARE z INTEGER; BEGIN BEGIN z := 1 / 0; EXCEPTION WHEN division_by_zero THEN NEW.n := -1; END; RETURN NEW; END",
    );
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "catch_eval");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![-1]);

    create_fn(
        &core,
        "t1",
        "catch_after_write",
        "BEGIN BEGIN INSERT INTO audit (n) VALUES (NEW.n); RAISE EXCEPTION 'x'; \
         EXCEPTION WHEN OTHERS THEN NEW.n := -2; END; RETURN NEW; END",
    );
    ok(&core, "t1", "DROP TRIGGER tb ON s.tt");
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "catch_after_write");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (2)"),
        "0A000"
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![-1]);
    assert_eq!(audit_count(&core, "t1"), 0);
}

// (14) 末尾 `;` 付きの文でも発火する。
#[test]
fn trailing_semicolon_statement_fires() {
    let (core, _g) = core_with("trginv-semicolon", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "log_n",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END",
    );
    create_trigger(&core, "t1", "ta", "AFTER", "s.tt", "log_n");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (3);");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![3]);
}

// (8) 発火元の主体の写しが無い経路（セッション無し）で発火したトリガは、固定文言の `0A000` で拒否する。
// 窓口を直接組み立てて `TriggerHost` を呼ぶ。
//
// intentional: docs/design/plpgsql-subset.md
// 主体を推測して既定セッションで実行する fail-open を作らない（設計記録「Issue #3184 追記」）。
#[test]
fn missing_subject_scope_is_rejected() {
    use crate::catalog::trigger::{TriggerLevel, TriggerTiming};
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::row_codec::Value;
    use crate::trigger::call::{TriggerCall, TriggerHost, TriggerOp};

    let (core, _g) = core_with("trginv-nosubject", LedgerMode::Optional);
    base_tables(&core);
    create_fn(&core, "t1", "f", "BEGIN RETURN NEW; END");
    let pctx = ctx("t1");
    let txn = core.storage.begin_write_txn().expect("txn");
    let handle = crate::trigger::write_handle::TriggerWriteHandle::new(&txn, &pctx, None);
    let schema = TableSchema::new("tt", vec![ColumnDef::new("n", ColumnType::Integer, true)]);
    let new_row = [Value::Integer(1)];
    let call = TriggerCall {
        trigger_name: "tb",
        function_key: "f",
        table: "tt",
        schema: &schema,
        op: TriggerOp::Insert,
        level: TriggerLevel::Row,
        old_row: None,
        new_row: &new_row,
        args: &[],
        timing: TriggerTiming::Before,
        writes: &handle,
        host: Some(&core),
    };
    let err = core.invoke_trigger(&call).expect_err("no subject scope");
    assert_eq!(err.wire_code(), "0A000");
}

fn make_role(storage: &Storage, name: &str) -> u64 {
    use crate::catalog::role::{RoleAttributes, RoleDef};
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
}

fn bound_session(role: u64, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(role, role);
    assert!(s.bind_resolution_tenant("t1"));
    if ddl {
        s.allow_ddl();
    }
    s
}

// (7) 権限: 本体の書き込みは発火元のロールで判定される。監査表への INSERT 権限を持たないロールが
// 発火させると `42501` で文全体が失敗し、権限を付与すれば同じ構成で成功する（権限は広がりも狭まりもしない）。
#[test]
fn body_writes_are_checked_against_the_firing_role() {
    let path = unique_db_path("trginv-priv");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    let admin = make_role(&storage, "admin_r");
    let alice = make_role(&storage, "alice");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let c = ctx("t1");
    let admin_run = |sql: &str| {
        let mut s = bound_session(admin, true);
        core.execute_sql_in_session(&c, &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    };
    let alice_code = |sql: &str| {
        let mut s = bound_session(alice, false);
        match core.execute_sql_in_session(&c, &mut s, sql) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.wire_code().to_string(),
        }
    };
    admin_run("CREATE SCHEMA s");
    admin_run("CREATE TABLE s.tt (n INTEGER)");
    admin_run("CREATE TABLE audit (n INTEGER)");
    admin_run(
        "CREATE FUNCTION log_n() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END $$",
    );
    admin_run("CREATE TRIGGER ta AFTER INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION log_n()");
    admin_run("GRANT USAGE ON SCHEMA s TO alice");
    admin_run("GRANT INSERT ON s.tt TO alice");
    assert_eq!(alice_code("INSERT INTO s.tt (n) VALUES (1)"), "42501");
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert_eq!(audit_count(&core, "t1"), 0);
    admin_run("GRANT INSERT ON audit TO alice");
    assert_eq!(alice_code("INSERT INTO s.tt (n) VALUES (1)"), "ok");
    assert_eq!(audit_count(&core, "t1"), 1);
}

// 外側が DELETE でも、本体の INSERT は通常の INSERT と同じく採番文脈（`nextval`）と既定時刻の文脈
// （VALUES の `now()`）を持つ（PR #3369 レビュー指摘）。
#[test]
fn body_insert_has_sequence_and_clock_scopes_when_outer_is_delete() {
    let (core, _g) = core_with("trginv-scopes", LedgerMode::Optional);
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.tt (n INTEGER)");
    ok(&core, "t1", "CREATE TABLE audit (n INTEGER, ts TIMESTAMP)");
    ok(&core, "t1", "CREATE SEQUENCE aq");
    create_fn(
        &core,
        "t1",
        "del_audit",
        "BEGIN INSERT INTO audit (n, ts) VALUES (nextval('aq'), now()); RETURN NULL; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER t_del AFTER DELETE ON s.tt FOR EACH ROW EXECUTE FUNCTION del_audit()",
    );
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (2)");
    ok(&core, "t1", "DELETE FROM s.tt WHERE n = 1");
    ok(&core, "t1", "DELETE FROM s.tt WHERE n = 2");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1, 2]);
}

// 外側が DELETE（時刻の文脈なし）でも、本体の `now()` は VALUES だけでなく代入・`FROM` なしの
// `SELECT` でも同じく受理される（PR #3369 レビュー指摘。式スコープの時刻もフォールバックで張る）。
#[test]
fn body_now_outside_values_is_accepted_when_outer_is_delete() {
    let (core, _g) = core_with("trginv-now-forms", LedgerMode::Optional);
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.tt (n INTEGER)");
    ok(&core, "t1", "CREATE TABLE audit (n INTEGER)");
    for (i, body) in [
        "DECLARE t TIMESTAMP; BEGIN t := now(); INSERT INTO audit (n) VALUES (1); RETURN NULL; END",
        "BEGIN PERFORM now(); EXECUTE 'SELECT now()'; INSERT INTO audit (n) VALUES (2); RETURN NULL; END",
    ]
    .iter()
    .enumerate()
    {
        let f = format!("f{i}");
        create_fn(&core, "t1", &f, body);
        ok(
            &core,
            "t1",
            &format!("CREATE TRIGGER g{i} AFTER DELETE ON s.tt FOR EACH ROW EXECUTE FUNCTION {f}()"),
        );
    }
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    ok(&core, "t1", "DELETE FROM s.tt WHERE n = 1");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1, 2]);
}
/// 明示 Tx 内で `stmts` を実行し、続けて `SELECT now()` を返す（`BEGIN` の時刻と本体の時刻を比べる）。
fn in_txn_then_now(core: &EngineCore, stmts: &[&str]) -> Cell {
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    // `BEGIN` との時刻差が出るよう、本体の実行まで少し待つ。
    std::thread::sleep(std::time::Duration::from_millis(20));
    for sql in stmts {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    let now = match core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT now()")
        .expect("now")
    {
        SqlOutcome::Query(q) => q.rows[0].cells[0].clone(),
        other => panic!("expected query, got {other:?}"),
    };
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "COMMIT")
        .expect("commit");
    now
}

// Issue #3184（レビュー指摘）: `TRUNCATE` の文トリガ本体の `now()` は明示 Tx の `BEGIN` 時刻と一致する。
#[test]
fn truncate_trigger_shares_explicit_txn_start_time() {
    let (core, _g) = core_with("trginv-trunc-clock", LedgerMode::Optional);
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.tt (n INTEGER)");
    ok(&core, "t1", "CREATE TABLE audit_ts (ts TIMESTAMP)");
    create_fn(
        &core,
        "t1",
        "trunc_audit",
        "BEGIN INSERT INTO audit_ts (ts) VALUES (now()); RETURN NULL; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER t_trunc AFTER TRUNCATE ON s.tt FOR EACH STATEMENT EXECUTE FUNCTION trunc_audit()",
    );
    let now = in_txn_then_now(&core, &["TRUNCATE s.tt"]);
    let r = rows(&core, "t1", "SELECT ts FROM audit_ts");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], now, "trigger now() must equal the BEGIN time");
}

// Issue #3184（レビュー指摘）: 1 本体内の複数 `INSERT` は同じ `now()` を共有し、`DELETE` 起点でも `BEGIN` 時刻になる。
#[test]
fn delete_trigger_inserts_share_one_time_in_explicit_txn() {
    let (core, _g) = core_with("trginv-del-clock", LedgerMode::Optional);
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.tt (n INTEGER)");
    ok(&core, "t1", "CREATE TABLE audit_ts (ts TIMESTAMP)");
    create_fn(
        &core,
        "t1",
        "del_audit",
        "BEGIN INSERT INTO audit_ts (ts) VALUES (now()); \
         INSERT INTO audit_ts (ts) VALUES (now()); RETURN NULL; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER t_del AFTER DELETE ON s.tt FOR EACH STATEMENT EXECUTE FUNCTION del_audit()",
    );
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    let now = in_txn_then_now(&core, &["DELETE FROM s.tt WHERE n = 1"]);
    let r = rows(&core, "t1", "SELECT ts FROM audit_ts");
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], now);
    assert_eq!(r[1][0], now);
}
// ---- Issue #3186: トリガ本体の `UPDATE`（窓口経由）----

/// 任意のイベント（`AFTER UPDATE` 等）を指定して行トリガを作る。
fn create_trigger_on(core: &EngineCore, tenant: &str, trg: &str, spec: &str, table: &str, f: &str) {
    ok(
        core,
        tenant,
        &format!("CREATE TRIGGER {trg} {spec} ON {table} FOR EACH ROW EXECUTE FUNCTION {f}()"),
    );
}

/// 表 `counters (k, c)` を作り、`k` ごとに 1 行（c = 0）入れる。
fn counters(core: &EngineCore, keys: &[i64]) {
    ok(core, "t1", "CREATE TABLE counters (k INTEGER, c INTEGER)");
    for k in keys {
        ok(
            core,
            "t1",
            &format!("INSERT INTO counters (k, c) VALUES ({k}, 0)"),
        );
    }
}

fn counter_of(core: &EngineCore, tenant: &str, k: i64) -> i64 {
    ints(
        core,
        tenant,
        &format!("SELECT c FROM counters WHERE k = {k}"),
    )[0]
}

// (1)(4) 本体の `UPDATE`（述語形・変数参照・末尾 `;`）が反映される。複数行 VALUES は行ごとに加算する。
// 単一行 `id` 指定形も反映される。
#[test]
fn body_update_applies_for_predicate_and_single_row_forms() {
    let (core, _g) = core_with("trginv-upd-basic", LedgerMode::Optional);
    base_tables(&core);
    counters(&core, &[1, 2]);
    create_fn(
        &core,
        "t1",
        "bump",
        "BEGIN UPDATE counters SET c = c + 1 WHERE k = NEW.n; RETURN NEW; END",
    );
    create_trigger(&core, "t1", "ta", "AFTER", "s.tt", "bump");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1), (1), (2);");
    assert_eq!(counter_of(&core, "t1", 1), 2);
    assert_eq!(counter_of(&core, "t1", 2), 1);

    // `id` 指定の単一行形は、`id` を持つ従来方式の表に対して使う（ストレージ経由で作る）。
    core.storage
        .create_table(&crate::catalog::TableSchema::new(
            "sid",
            vec![crate::catalog::ColumnDef::new(
                "n",
                crate::catalog::ColumnType::Integer,
                true,
            )],
        ))
        .expect("create sid");
    ok(&core, "t1", "INSERT INTO sid (id, n) VALUES (5, 1)");
    create_fn(
        &core,
        "t1",
        "by_id",
        "BEGIN UPDATE sid SET n = 7 WHERE id = 5; RETURN NEW; END",
    );
    ok(&core, "t1", "DROP TRIGGER ta ON s.tt");
    create_trigger(&core, "t1", "tb", "AFTER", "s.tt", "by_id");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (9)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM sid"), vec![7]);
}

// (1) `FOUND` を使う upsert 型の分岐。
#[test]
fn body_update_sets_found_for_upsert() {
    let (core, _g) = core_with("trginv-upd-found", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "CREATE TABLE counters (k INTEGER, c INTEGER)");
    create_fn(
        &core,
        "t1",
        "upsert",
        "BEGIN UPDATE counters SET c = c + 1 WHERE k = NEW.n; \
         IF NOT FOUND THEN INSERT INTO counters (k, c) VALUES (NEW.n, 1); END IF; RETURN NEW; END",
    );
    create_trigger(&core, "t1", "ta", "AFTER", "s.tt", "upsert");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (5)");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (5)");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (6)");
    assert_eq!(counter_of(&core, "t1", 5), 2);
    assert_eq!(counter_of(&core, "t1", 6), 1);
}

// (1) 外側が UPDATE／DELETE の AFTER ROW から別表を更新する（OLD・NEW 参照）。明示 Tx の ROLLBACK で本体の更新も消える。
#[test]
fn body_update_from_update_and_delete_triggers_rolls_back_with_the_txn() {
    let (core, _g) = core_with("trginv-upd-outer", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (2)");
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (2)");
    create_fn(
        &core,
        "t1",
        "on_upd",
        "BEGIN UPDATE audit SET n = NEW.n WHERE n = OLD.n; RETURN NEW; END",
    );
    create_fn(
        &core,
        "t1",
        "on_del",
        "BEGIN UPDATE audit SET n = -1 WHERE n = OLD.n; RETURN OLD; END",
    );
    create_trigger_on(&core, "t1", "tu", "AFTER UPDATE", "s.tt", "on_upd");
    create_trigger_on(&core, "t1", "td", "AFTER DELETE", "s.tt", "on_del");
    let sorted = |core: &EngineCore| {
        let mut v = ints(core, "t1", "SELECT n FROM audit");
        v.sort_unstable();
        v
    };
    ok(&core, "t1", "UPDATE s.tt SET n = 10 WHERE n = 1");
    ok(&core, "t1", "DELETE FROM s.tt WHERE n = 2");
    assert_eq!(sorted(&core), vec![-1, 10]);

    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "UPDATE s.tt SET n = 20 WHERE n = 10")
        .expect("update in txn");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(sorted(&core), vec![-1, 10]);
}

// (1)(5) テナント境界: 同名・同値の表を持つ t2 の行は t1 のトリガ本体の `UPDATE` で変わらず、t2 の文では t1 のトリガは発火しない。
#[test]
fn body_update_never_touches_another_tenant() {
    let (core, _g) = core_with("trginv-upd-tenant", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    ok(&core, "t2", "INSERT INTO audit (n) VALUES (1)");
    create_fn(
        &core,
        "t1",
        "zero",
        "BEGIN UPDATE audit SET n = 0 WHERE n > 0; RETURN NEW; END",
    );
    create_trigger(&core, "t1", "ta", "AFTER", "s.tt", "zero");
    ok(&core, "t2", "CREATE SCHEMA s");
    ok(&core, "t2", "CREATE TABLE s.tt (n INTEGER)");
    ok(&core, "t2", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    assert_eq!(ints(&core, "t2", "SELECT n FROM audit"), vec![1]);
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![0]);
    assert_eq!(ints(&core, "t2", "SELECT n FROM audit"), vec![1]);
}

// (1)(5) 行ポリシー: 発火元の主体で UPDATE の `USING` が効き、不可視の行は更新されない。`WITH CHECK` 違反は文全体が失敗する。
#[test]
fn body_update_honors_row_policies() {
    let (core, _g) = core_with("trginv-upd-rls", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (200)");
    ok(&core, "t1", "ALTER TABLE audit ENABLE ROW LEVEL SECURITY");
    ok(&core, "t1", "ALTER TABLE audit FORCE ROW LEVEL SECURITY");
    ok(
        &core,
        "t1",
        "CREATE POLICY sel ON audit FOR SELECT USING (true)",
    );
    ok(
        &core,
        "t1",
        "CREATE POLICY upd ON audit FOR UPDATE USING (n < 100) WITH CHECK (n < 100)",
    );
    create_fn(
        &core,
        "t1",
        "inc",
        "BEGIN UPDATE audit SET n = n + 1 WHERE n >= 0; RETURN NEW; END",
    );
    create_trigger(&core, "t1", "ta", "AFTER", "s.tt", "inc");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    let mut v = ints(&core, "t1", "SELECT n FROM audit");
    v.sort_unstable();
    assert_eq!(v, vec![2, 200], "the invisible row must not be updated");

    create_fn(
        &core,
        "t1",
        "bad",
        "BEGIN UPDATE audit SET n = 500 WHERE n < 100; RETURN NEW; END",
    );
    ok(&core, "t1", "DROP TRIGGER ta ON s.tt");
    create_trigger(&core, "t1", "tb", "AFTER", "s.tt", "bad");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (2)"),
        "42501"
    );
    let mut v = ints(&core, "t1", "SELECT n FROM audit");
    v.sort_unstable();
    assert_eq!(v, vec![2, 200]);
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt").len(), 1);
}

// (5) 権限: 本体の `UPDATE` は発火元ロールで判定される。権限が無ければ `42501` で文全体が失敗し、付与すれば成功する。
#[test]
fn body_update_is_checked_against_the_firing_role() {
    let path = unique_db_path("trginv-upd-priv");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    let admin = make_role(&storage, "admin_r");
    let alice = make_role(&storage, "alice");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let c = ctx("t1");
    let admin_run = |sql: &str| {
        let mut s = bound_session(admin, true);
        core.execute_sql_in_session(&c, &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    };
    let alice_code = |sql: &str| {
        let mut s = bound_session(alice, false);
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
        "CREATE FUNCTION inc() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN UPDATE audit SET n = n + 1 WHERE n >= 0; RETURN NEW; END $$",
    );
    admin_run("CREATE TRIGGER ta AFTER INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION inc()");
    admin_run("GRANT USAGE ON SCHEMA s TO alice");
    admin_run("GRANT INSERT ON s.tt TO alice");
    assert_eq!(alice_code("INSERT INTO s.tt (n) VALUES (1)"), "42501");
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
    admin_run("GRANT UPDATE ON audit TO alice");
    admin_run("GRANT SELECT ON audit TO alice");
    assert_eq!(alice_code("INSERT INTO s.tt (n) VALUES (1)"), "ok");
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![2]);
}

// (5) STABLE のトリガ関数の本体 `UPDATE` は揮発性ゲートで拒否する。
//
// intentional: docs/design/plpgsql-subset.md
// PG17 と同じく、STABLE／IMMUTABLE 関数のデータ変更は拒否する（設計記録の揮発性ゲート）。
#[test]
fn stable_trigger_function_cannot_update() {
    let (core, _g) = core_with("trginv-upd-stable", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    ok(
        &core,
        "t1",
        "CREATE FUNCTION st() RETURNS trigger LANGUAGE plpgsql STABLE AS $$ \
         BEGIN UPDATE audit SET n = 9 WHERE n >= 0; RETURN NEW; END $$",
    );
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "st");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
        "0A000"
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
}

// 注記（Issue #3361）: 本体の `UPDATE` の書き込み先は共有 `public` 表の裸の名前に限られる（SCHEMA 修飾は窓口の
// 許可リスト外）一方、共有 `public` 表のトリガは定義時に拒否される。そのため「トリガ表と本体の更新先が同じ表」
// になる形（自己・相互再帰の深さ上限 `54001`、BEFORE ROW の行ごとループ中の同表 `UPDATE` の D6 安全弁）は
// SQL から構成できない。深さ上限は注入 invoker の `core/trigger_depth_tests.rs` が、D6 の判定は
// `tenant/trigger_row_loop.rs` が担う。本体の書き込み先に SCHEMA 修飾を許す拡張が入った時点で、本体 SQL 経由の
// 検証をここへ戻す（スコープ外。返却の outOfScope に記録）。

// (3) 台帳: 外側の `operation_id` の 1 エントリだけが載る。本体の `UPDATE` は `operation_id` を要求も記録もせず、
// 再送は `23505` で本体を再実行しない。
#[test]
fn ledgered_core_body_update_does_not_touch_the_ledger() {
    let (core, _g) = core_with("trginv-upd-ledger", LedgerMode::Ledgered);
    base_tables(&core);
    ok(&core, "t1", "CREATE TABLE counters (k INTEGER, c INTEGER)");
    ok(
        &core,
        "t1",
        "INSERT INTO counters (k, c) VALUES (1, 0) USING OPERATION_ID 'seed'",
    );
    create_fn(
        &core,
        "t1",
        "bump",
        "BEGIN UPDATE counters SET c = c + 1 WHERE k >= 0; RETURN NEW; END",
    );
    create_trigger(&core, "t1", "ta", "AFTER", "s.tt", "bump");
    let sql = "INSERT INTO s.tt (n) VALUES (1) USING OPERATION_ID 'op-1'";
    ok(&core, "t1", sql);
    assert_eq!(counter_of(&core, "t1", 1), 1);
    assert_eq!(code(&core, "t1", sql), "23505");
    assert_eq!(
        counter_of(&core, "t1", 1),
        1,
        "resend must not rerun the body"
    );
}

// (5) EXCEPTION 節: `UPDATE` を試みた後のエラーは捕捉せず `0A000`、文全体が取り消される。
//
// intentional: docs/design/plpgsql-subset.md
// 書き込み後の EXCEPTION 捕捉は、窓口が savepoint を持たないための既知の差（設計記録「Issue #3184 追記」）。
#[test]
fn exception_block_does_not_catch_after_a_body_update() {
    let (core, _g) = core_with("trginv-upd-exception", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    create_fn(
        &core,
        "t1",
        "catch_after_update",
        "BEGIN BEGIN UPDATE audit SET n = 9 WHERE n >= 0; RAISE EXCEPTION 'x'; \
         EXCEPTION WHEN OTHERS THEN NEW.n := -2; END; RETURN NEW; END",
    );
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "catch_after_update");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.tt (n) VALUES (2)"),
        "0A000"
    );
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1]);
}

// (5)(6) 窓口が受けない `UPDATE` の形は、PG とは異なる既知の差として固定の `0A000` で拒否し、副作用を残さない。
//
// intentional: docs/design/plpgsql-subset.md
// `RETURNING`・WHERE 内のサブクエリ（表の読み取りは #3187 の範囲）・`UPDATE ... FROM`・ビュー・SCHEMA 修飾の書き込み先は
// 窓口の許可リストに無い形（設計記録「Issue #3186 追記」）。
#[test]
fn update_forms_outside_the_write_handle_are_rejected() {
    let (core, _g) = core_with("trginv-upd-forms", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "INSERT INTO audit (n) VALUES (1)");
    ok(&core, "t1", "CREATE VIEW va AS SELECT id, n FROM audit");
    ok(&core, "t1", "CREATE TABLE src (n INTEGER)");
    let forms = [
        "UPDATE audit SET n = 9 WHERE n >= 0 RETURNING n",
        "UPDATE audit SET n = 9 WHERE n IN (SELECT n FROM src)",
        "UPDATE audit SET n = 9 FROM src WHERE audit.n = src.n",
        "UPDATE va SET n = 9 WHERE n >= 0",
    ];
    for (i, stmt) in forms.iter().enumerate() {
        let f = format!("uform{i}");
        create_fn(&core, "t1", &f, &format!("BEGIN {stmt}; RETURN NEW; END"));
        ok(&core, "t1", "DROP TRIGGER IF EXISTS tb ON s.tt");
        create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", &f);
        assert_eq!(
            code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
            "0A000",
            "{stmt}"
        );
        assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty(), "{stmt}");
        assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1], "{stmt}");
    }
}

// (1)(5) 遅延 FK: 本体の `UPDATE` が子の FK 列を存在しない親キーへ変える、または参照中の親キーを変えると、
// autocommit は文末、明示 Tx は `COMMIT` で `23503` になる（書いた表の記録漏れの検出）。
#[test]
fn body_update_is_checked_by_deferred_foreign_keys() {
    let (core, _g) = core_with("trginv-upd-fk", LedgerMode::Optional);
    base_tables(&core);
    ok(&core, "t1", "CREATE TABLE fk_parent (k BIGINT PRIMARY KEY)");
    ok(
        &core,
        "t1",
        "CREATE TABLE fk_child (parent_id BIGINT REFERENCES fk_parent DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(&core, "t1", "INSERT INTO fk_parent (k) VALUES (1), (2)");
    ok(&core, "t1", "INSERT INTO fk_child (parent_id) VALUES (1)");
    for (i, body) in [
        "UPDATE fk_child SET parent_id = 99 WHERE parent_id >= 0",
        "UPDATE fk_parent SET k = 50 WHERE k = 1",
    ]
    .iter()
    .enumerate()
    {
        let f = format!("fk{i}");
        create_fn(&core, "t1", &f, &format!("BEGIN {body}; RETURN NEW; END"));
        ok(&core, "t1", "DROP TRIGGER IF EXISTS ta ON s.tt");
        create_trigger(&core, "t1", "ta", "AFTER", "s.tt", &f);
        assert_eq!(
            code(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)"),
            "23503",
            "{body}"
        );
        assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty(), "{body}");
        assert_eq!(ints(&core, "t1", "SELECT parent_id FROM fk_child"), vec![1]);

        let c = ctx("t1");
        let mut s = session();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "INSERT INTO s.tt (n) VALUES (1)")
            .expect("deferred check waits for COMMIT");
        let e = core
            .execute_sql_in_txn(&c, &mut s, &mut txn, "COMMIT")
            .expect_err("deferred FK violation");
        assert_eq!(e.wire_code(), "23503", "{body}");
        assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty(), "{body}");
    }
}

// (5) 拒否のエラー文言は、他テナントの同名の表・関数の有無で変わらない。
#[test]
fn update_rejection_text_does_not_depend_on_other_tenants() {
    let (core, _g) = core_with("trginv-upd-errtext", LedgerMode::Optional);
    base_tables(&core);
    create_fn(
        &core,
        "t1",
        "ret",
        "BEGIN UPDATE audit SET n = 9 WHERE n >= 0 RETURNING n; RETURN NEW; END",
    );
    create_trigger(&core, "t1", "tb", "BEFORE", "s.tt", "ret");
    let before = run(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)")
        .unwrap_err()
        .client_message();
    ok(&core, "t2", "INSERT INTO audit (n) VALUES (1)");
    create_fn(&core, "t2", "ret", "BEGIN RETURN NEW; END");
    let after = run(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)")
        .unwrap_err()
        .client_message();
    assert_eq!(before, after);
}
