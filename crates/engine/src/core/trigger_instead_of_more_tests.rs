//! INSTEAD OF ROW トリガの結合テスト（続き。TABLE-45・TABLE-43・Issue #3193）。優先順位・行意味論・
//! 原子性・明示 Tx・定義の形・fail-closed・テナント境界。ヘルパーは `trigger_instead_of_tests` と共有する。

use super::trigger_instead_of_tests::{
    affected, audit, audit_triggers, code, core_with, count, create_fn, ctx, ints, ok, rows,
    session, setup,
};

// 更新可能ビューでもトリガがあればトリガを優先し、DROP すると自動書き換え経路へ戻る。
#[test]
fn trigger_takes_precedence_over_auto_rewrite() {
    let (core, _g) = core_with("instof-prec");
    setup(&core);
    create_fn(
        &core,
        "t1",
        "f_ins",
        "BEGIN INSERT INTO audit (tag, n) VALUES ('ins', NEW.n); RETURN NEW; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER g_ins INSTEAD OF INSERT ON s.uv FOR EACH ROW EXECUTE FUNCTION f_ins()",
    );
    assert_eq!(count(&core, "t1", "INSERT INTO s.uv (n) VALUES (7)"), 1);
    // 基表には書かれず、監査表にだけ載る。
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
    assert_eq!(audit(&core, "ins"), vec![7]);
    // INSERT 用の INSTEAD OF しか無い間、UPDATE は従来の自動書き換え（基表が空なので 0 行）。
    assert_eq!(count(&core, "t1", "UPDATE s.uv SET n = 1"), 0);

    ok(&core, "t1", "DROP TRIGGER g_ins ON s.uv");
    assert_eq!(count(&core, "t1", "INSERT INTO s.uv (n) VALUES (8)"), 1);
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![8]);
}

// `NULL` 返却は処理済みに数えない。複数トリガは名前順に連鎖し、INSERT の NEW が引き継がれる。
#[test]
fn null_return_skips_and_triggers_chain_by_name() {
    let (core, _g) = core_with("instof-chain");
    setup(&core);
    create_fn(&core, "t1", "f_null", "BEGIN RETURN NULL; END");
    ok(
        &core,
        "t1",
        "CREATE TRIGGER a_null INSTEAD OF INSERT ON s.nv FOR EACH ROW EXECUTE FUNCTION f_null()",
    );
    assert_eq!(
        count(&core, "t1", "INSERT INTO s.nv (n) VALUES (1), (2)"),
        0
    );
    ok(&core, "t1", "DROP TRIGGER a_null ON s.nv");

    create_fn(
        &core,
        "t1",
        "f_plus",
        "BEGIN NEW.n := NEW.n + 100; RETURN NEW; END",
    );
    create_fn(
        &core,
        "t1",
        "f_log",
        "BEGIN INSERT INTO audit (tag, n) VALUES ('ins', NEW.n); RETURN NEW; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER b_plus INSTEAD OF INSERT ON s.nv FOR EACH ROW EXECUTE FUNCTION f_plus()",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER c_log INSTEAD OF INSERT ON s.nv FOR EACH ROW EXECUTE FUNCTION f_log()",
    );
    assert_eq!(
        count(&core, "t1", "INSERT INTO s.nv (n) VALUES (1), (2)"),
        2
    );
    assert_eq!(audit(&core, "ins"), vec![101, 102]);
}

// トリガ内の例外は文全体を取り消す（複数行 INSERT の途中失敗でも先行行の副作用は残らない）。
#[test]
fn trigger_error_rolls_back_whole_statement() {
    let (core, _g) = core_with("instof-atomic");
    setup(&core);
    create_fn(
        &core,
        "t1",
        "f_ins",
        "BEGIN IF NEW.n = 2 THEN RAISE EXCEPTION 'boom'; END IF; \
         INSERT INTO audit (tag, n) VALUES ('ins', NEW.n); RETURN NEW; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER g_ins INSTEAD OF INSERT ON s.nv FOR EACH ROW EXECUTE FUNCTION f_ins()",
    );
    assert_ne!(
        code(&core, "t1", "INSERT INTO s.nv (n) VALUES (1), (2), (3)"),
        "ok"
    );
    assert!(audit(&core, "ins").is_empty());
    assert_eq!(
        count(&core, "t1", "INSERT INTO s.nv (n) VALUES (1), (3)"),
        2
    );
    assert_eq!(audit(&core, "ins"), vec![1, 3]);
}

// 明示 Tx: ROLLBACK で取り消され、COMMIT で確定する。Tx 内の先行書き込みが OLD 行の取得に見える。
#[test]
fn explicit_transaction_sees_earlier_writes_and_rolls_back() {
    let (core, _g) = core_with("instof-tx");
    setup(&core);
    audit_triggers(&core, "t1", "s.nv");
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    let mut exec = |s: &mut crate::sql::mode::SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
    };
    exec(&mut s, "BEGIN").expect("begin");
    exec(&mut s, "INSERT INTO s.tt (n) VALUES (5)").expect("base insert");
    let out = exec(&mut s, "DELETE FROM s.nv WHERE n = 5").expect("delete in tx");
    assert_eq!(affected(out), 1);
    exec(&mut s, "ROLLBACK").expect("rollback");
    assert!(audit(&core, "del").is_empty());
    assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());

    exec(&mut s, "BEGIN").expect("begin");
    let out = exec(&mut s, "INSERT INTO s.nv (n) VALUES (9), (10)").expect("view insert in tx");
    assert_eq!(affected(out), 2);
    exec(&mut s, "COMMIT").expect("commit");
    assert_eq!(audit(&core, "ins"), vec![9, 10]);
}

// 明示 Tx 内のビュー INSERT 経由のトリガが書く表の DEFAULT now() は、ビュー INSERT 時点ではなく
// Tx 開始時刻になる（同一 Tx の通常 INSERT と同じ値。既存の時刻契約を引き継ぐ。Issue #3193）。
#[test]
fn explicit_transaction_view_insert_keeps_transaction_start_time() {
    let (core, _g) = core_with("instof-tx-clock");
    setup(&core);
    ok(
        &core,
        "t1",
        "CREATE TABLE ev (tag TEXT, ts TIMESTAMP DEFAULT now())",
    );
    create_fn(
        &core,
        "t1",
        "f_ev",
        "BEGIN INSERT INTO ev (tag) VALUES ('via_view'); RETURN NEW; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER g_ev INSTEAD OF INSERT ON s.nv FOR EACH ROW EXECUTE FUNCTION f_ev()",
    );
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    let mut exec = |s: &mut crate::sql::mode::SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
    };
    exec(&mut s, "BEGIN").expect("begin");
    exec(&mut s, "INSERT INTO ev (tag) VALUES ('direct')").expect("direct insert");
    std::thread::sleep(std::time::Duration::from_millis(30));
    exec(&mut s, "INSERT INTO s.nv (n) VALUES (1)").expect("view insert");
    exec(&mut s, "COMMIT").expect("commit");
    let direct = rows(&core, "t1", "SELECT ts FROM ev WHERE tag = 'direct'");
    let via = rows(&core, "t1", "SELECT ts FROM ev WHERE tag = 'via_view'");
    assert_eq!(direct.len(), 1);
    assert_eq!(via.len(), 1);
    assert_eq!(format!("{:?}", direct[0][0]), format!("{:?}", via[0][0]));
}

// 明示 Tx でトリガ本体が失敗すると Tx は Failed（25P02）になる。
#[test]
fn explicit_transaction_trigger_failure_fails_the_transaction() {
    let (core, _g) = core_with("instof-tx-fail");
    setup(&core);
    create_fn(&core, "t1", "f_bad", "BEGIN RAISE EXCEPTION 'boom'; END");
    ok(
        &core,
        "t1",
        "CREATE TRIGGER g INSTEAD OF INSERT ON s.nv FOR EACH ROW EXECUTE FUNCTION f_bad()",
    );
    let c = ctx("t1");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    assert!(core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "INSERT INTO s.nv (n) VALUES (1)")
        .is_err());
    let err = core
        .execute_sql_in_txn(
            &c,
            &mut s,
            &mut txn,
            "INSERT INTO audit (tag, n) VALUES ('x', 1)",
        )
        .expect_err("failed tx rejects statements");
    assert_eq!(err.wire_code(), "25P02");
}

// 定義の形: ROW 必須・TRUNCATE／UPDATE OF／WHEN 不可・テーブルは 42809・BEFORE/AFTER のビューは 42809。
#[test]
fn definition_shapes_are_fail_closed() {
    let (core, _g) = core_with("instof-ddl");
    setup(&core);
    create_fn(&core, "t1", "f", "BEGIN RETURN NEW; END");
    let create = |rest: &str| {
        code(
            &core,
            "t1",
            &format!("CREATE TRIGGER g {rest} EXECUTE FUNCTION f()"),
        )
    };
    assert_eq!(create("INSTEAD OF INSERT ON s.nv"), "0A000");
    assert_eq!(
        create("INSTEAD OF INSERT ON s.nv FOR EACH STATEMENT"),
        "0A000"
    );
    assert_eq!(
        create("INSTEAD OF UPDATE OF n ON s.nv FOR EACH ROW"),
        "0A000"
    );
    assert_eq!(create("INSTEAD OF INSERT ON s.tt FOR EACH ROW"), "42809");
    assert_eq!(create("BEFORE INSERT ON s.nv FOR EACH ROW"), "42809");
    assert_eq!(create("AFTER INSERT ON s.nv FOR EACH ROW"), "42809");
    assert_eq!(
        create("INSTEAD OF INSERT ON s.missing FOR EACH ROW"),
        "42P01"
    );
    assert_eq!(create("INSTEAD OF INSERT ON s.nv FOR EACH ROW"), "ok");
    // 同名は 42710、OR REPLACE は置き換え。
    assert_eq!(create("INSTEAD OF INSERT ON s.nv FOR EACH ROW"), "42710");
    assert_eq!(
        code(
            &core,
            "t1",
            "CREATE OR REPLACE TRIGGER g INSTEAD OF DELETE ON s.nv FOR EACH ROW EXECUTE FUNCTION f()"
        ),
        "ok"
    );
}

// 共有 public ビューへの INSTEAD OF は定義時に 0A000（全テナントの書き込みを表現できないため）。
#[test]
fn shared_public_view_is_rejected_at_definition() {
    let (core, _g) = core_with("instof-public");
    ok(&core, "t1", "CREATE TABLE pt (n INTEGER)");
    ok(&core, "t1", "CREATE VIEW pv AS SELECT n FROM pt LIMIT 10");
    create_fn(&core, "t1", "f", "BEGIN RETURN NEW; END");
    assert_eq!(
        code(
            &core,
            "t1",
            "CREATE TRIGGER g INSTEAD OF INSERT ON pv FOR EACH ROW EXECUTE FUNCTION f()"
        ),
        "0A000"
    );
}

// 未対応の形（RETURNING）は固定の 0A000。トリガ無しのビューは従来どおり 55000
// （他テナントが同名のビューを持っていても応答は変わらない）。
#[test]
fn unsupported_forms_are_0a000_and_no_trigger_stays_55000() {
    let (core, _g) = core_with("instof-forms");
    setup(&core);
    audit_triggers(&core, "t1", "s.nv");
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.nv (n) VALUES (1) RETURNING n"),
        "0A000"
    );
    assert_eq!(
        code(&core, "t1", "DELETE FROM s.nv WHERE n = 1 RETURNING n"),
        "0A000"
    );
    assert_eq!(
        code(&core, "t1", "UPDATE s.nv SET n = 2 WHERE n = 1 RETURNING n"),
        "0A000"
    );
    assert!(audit(&core, "ins").is_empty());
    ok(&core, "t2", "CREATE SCHEMA s");
    ok(&core, "t2", "CREATE TABLE s.tt (n INTEGER)");
    ok(
        &core,
        "t2",
        "CREATE VIEW s.nv AS SELECT n FROM s.tt LIMIT 1000",
    );
    assert_eq!(
        code(&core, "t2", "INSERT INTO s.nv (n) VALUES (1)"),
        "55000"
    );
    assert_eq!(code(&core, "t2", "DELETE FROM s.nv"), "55000");
    assert_eq!(code(&core, "t2", "UPDATE s.nv SET n = 1"), "55000");
}

// DROP VIEW でトリガも消え、同名のビューを作り直しても旧定義は復活しない。DROP FUNCTION は
// トリガ参照があれば拒否される。
#[test]
fn drop_view_removes_triggers_and_function_is_protected() {
    let (core, _g) = core_with("instof-drop");
    setup(&core);
    audit_triggers(&core, "t1", "s.nv");
    assert_eq!(code(&core, "t1", "DROP FUNCTION f_ins"), "2BP01");
    ok(&core, "t1", "DROP VIEW s.nv");
    ok(
        &core,
        "t1",
        "CREATE VIEW s.nv AS SELECT n FROM s.tt LIMIT 1000",
    );
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.nv (n) VALUES (1)"),
        "55000"
    );
    ok(&core, "t1", "DROP FUNCTION f_ins");
}

// 式の SET・列別名付きビューが通る。列リスト省略の INSERT は従来どおり 0A000（ビューの公開列での展開が未対応。#1730）。
#[test]
fn insert_without_column_list_and_set_expressions() {
    let (core, _g) = core_with("instof-cols");
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.tt (a INTEGER, b TEXT)");
    ok(&core, "t1", "CREATE TABLE audit (tag TEXT, n INTEGER)");
    ok(
        &core,
        "t1",
        "CREATE VIEW s.v (x, y) AS SELECT a, b FROM s.tt LIMIT 100",
    );
    create_fn(
        &core,
        "t1",
        "f_ins",
        "BEGIN INSERT INTO audit (tag, n) VALUES (NEW.y, NEW.x); RETURN NEW; END",
    );
    create_fn(
        &core,
        "t1",
        "f_upd",
        "BEGIN INSERT INTO audit (tag, n) VALUES (NEW.y, NEW.x * 10 + OLD.x); RETURN NEW; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER g_ins INSTEAD OF INSERT ON s.v FOR EACH ROW EXECUTE FUNCTION f_ins()",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER g_upd INSTEAD OF UPDATE ON s.v FOR EACH ROW EXECUTE FUNCTION f_upd()",
    );
    assert_eq!(
        code(&core, "t1", "INSERT INTO s.v VALUES (1, 'one')"),
        "0A000"
    );
    assert_eq!(
        count(&core, "t1", "INSERT INTO s.v (y, x) VALUES ('two', 2)"),
        1
    );
    assert_eq!(
        count(&core, "t1", "INSERT INTO s.v (x, y) VALUES (1, 'one')"),
        1
    );
    ok(&core, "t1", "INSERT INTO s.tt (a, b) VALUES (3, 'three')");
    assert_eq!(
        count(&core, "t1", "UPDATE s.v SET x = x + 1 WHERE y = 'three'"),
        1
    );
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1, 2, 43]);
}

// 他テナントの行は OLD 行に現れない（RLS-10 (b)）。
#[test]
fn other_tenants_rows_never_reach_old() {
    let (core, _g) = core_with("instof-tenant");
    setup(&core);
    audit_triggers(&core, "t1", "s.nv");
    ok(&core, "t2", "CREATE SCHEMA s");
    ok(&core, "t2", "CREATE TABLE s.tt (n INTEGER)");
    ok(&core, "t2", "INSERT INTO s.tt (n) VALUES (999)");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    assert_eq!(count(&core, "t1", "DELETE FROM s.nv"), 1);
    assert_eq!(audit(&core, "del"), vec![1]);
}

// 更新可能ビューでも、INSTEAD OF トリガがあれば `WHERE` 内サブクエリ付きの UPDATE・DELETE が構文解析段で
// 拒否されず、トリガへ届く。トリガが無ければ自動書き換えの固定の拒否（`42601`）のまま。
#[test]
fn where_subquery_reaches_trigger_on_updatable_view() {
    let (core, _g) = core_with("instof-subq");
    setup(&core);
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (1)");
    ok(&core, "t1", "INSERT INTO s.tt (n) VALUES (2)");
    let upd = "UPDATE s.uv SET n = 9 WHERE n IN (SELECT n FROM s.tt WHERE n = 1)";
    let del = "DELETE FROM s.uv WHERE n IN (SELECT n FROM s.tt WHERE n = 2)";
    assert_eq!(code(&core, "t1", upd), "42601");
    assert_eq!(code(&core, "t1", del), "42601");
    audit_triggers(&core, "t1", "s.uv");
    assert_eq!(count(&core, "t1", upd), 1);
    assert_eq!(count(&core, "t1", del), 1);
    assert_eq!(audit(&core, "upd"), vec![9]);
    assert_eq!(audit(&core, "del"), vec![2]);
    // 基表には直接書かれない。
    assert_eq!(
        ints(&core, "t1", "SELECT n FROM s.tt ORDER BY n"),
        vec![1, 2]
    );
}

// 2 つの公開列が同じ基表の列へ写るビューでも、INSTEAD OF トリガがあれば INSERT・UPDATE は構文解析段で
// `42701` にならずトリガへ届く（各列は独立した NEW の列）。トリガが無ければ自動書き換えの `42701` のまま。
#[test]
fn duplicate_mapped_columns_reach_trigger() {
    let (core, _g) = core_with("instof-dup");
    setup(&core);
    ok(
        &core,
        "t1",
        "CREATE VIEW s.dv (a, b) AS SELECT n, n FROM s.tt",
    );
    let ins = "INSERT INTO s.dv (a, b) VALUES (1, 2)";
    let upd = "UPDATE s.dv SET a = 1, b = 2";
    assert_eq!(code(&core, "t1", ins), "42701");
    assert_eq!(code(&core, "t1", upd), "42701");
    create_fn(
        &core,
        "t1",
        "f_dup",
        "BEGIN INSERT INTO audit (tag, n) VALUES ('dup', NEW.a + NEW.b); RETURN NEW; END",
    );
    for ev in ["INSERT", "UPDATE"] {
        ok(
            &core,
            "t1",
            &format!(
                "CREATE TRIGGER g_{ev} INSTEAD OF {ev} ON s.dv FOR EACH ROW EXECUTE FUNCTION f_dup()"
            ),
        );
    }
    assert_eq!(count(&core, "t1", ins), 1);
    assert_eq!(audit(&core, "dup"), vec![3]);
}
