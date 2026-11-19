//! INSTEAD OF ROW トリガの結合テスト（形・権限・テナント。TABLE-45・TABLE-43・Issue #3193）。ヘルパーは
//! `trigger_instead_of_tests` と共有する。

use super::trigger_instead_of_tests::{
    audit, audit_triggers, code, core_with, count, create_fn, ctx, ints, ok, setup,
};

// D4: `ON CONFLICT`・`USING OPERATION_ID` は固定の 0A000（書き込み前に拒否し、監査表に何も載らない）。
// intentional: docs/design/trigger-instead-of.md
#[test]
fn conflict_and_operation_id_forms_are_0a000() {
    let (core, _g) = core_with("instof-d4");
    setup(&core);
    audit_triggers(&core, "t1", "s.nv");
    for sql in [
        "INSERT INTO s.nv (n) VALUES (1) ON CONFLICT DO NOTHING",
        "INSERT INTO s.nv (n) VALUES (1) USING OPERATION_ID 'op-1'",
        "DELETE FROM s.nv WHERE n = 1 USING OPERATION_ID 'op-2'",
        "UPDATE s.nv SET n = 2 WHERE n = 1 USING OPERATION_ID 'op-3'",
    ] {
        assert_eq!(code(&core, "t1", sql), "0A000", "{sql}");
    }
    assert!(audit(&core, "ins").is_empty());
}

// 列リストの一部だけを指定した INSERT では、省略した列は NULL でトリガへ渡る。
#[test]
fn partial_column_list_passes_null_for_omitted_columns() {
    let (core, _g) = core_with("instof-partial");
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.tt (a INTEGER, b INTEGER)");
    ok(&core, "t1", "CREATE TABLE audit (tag TEXT, n INTEGER)");
    ok(
        &core,
        "t1",
        "CREATE VIEW s.v AS SELECT a, b FROM s.tt LIMIT 10",
    );
    create_fn(
        &core,
        "t1",
        "f_ins",
        "BEGIN IF NEW.b IS NULL THEN INSERT INTO audit (tag, n) VALUES ('nullb', NEW.a); END IF; RETURN NEW; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER g INSTEAD OF INSERT ON s.v FOR EACH ROW EXECUTE FUNCTION f_ins()",
    );
    assert_eq!(count(&core, "t1", "INSERT INTO s.v (a) VALUES (4)"), 1);
    assert_eq!(audit(&core, "nullb"), vec![4]);
}

// 集計ビュー・JOIN ビュー（どちらも自動更新不可）でも行型の合成と OLD 行の取得ができる。
#[test]
fn aggregate_and_join_views_work() {
    let (core, _g) = core_with("instof-agg-join");
    ok(&core, "t1", "CREATE SCHEMA s");
    ok(&core, "t1", "CREATE TABLE s.a (k INTEGER, v INTEGER)");
    ok(&core, "t1", "CREATE TABLE s.b (k INTEGER, w INTEGER)");
    ok(&core, "t1", "CREATE TABLE audit (tag TEXT, n INTEGER)");
    ok(
        &core,
        "t1",
        "CREATE VIEW s.agg AS SELECT k, COUNT(*) AS c FROM s.a GROUP BY k",
    );
    ok(
        &core,
        "t1",
        "CREATE VIEW s.jv AS SELECT a.k, b.w FROM s.a INNER JOIN s.b ON a.k = b.k LIMIT 100",
    );
    create_fn(
        &core,
        "t1",
        "f_del",
        "BEGIN INSERT INTO audit (tag, n) VALUES ('del', OLD.k); RETURN OLD; END",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER g INSTEAD OF DELETE ON s.agg FOR EACH ROW EXECUTE FUNCTION f_del()",
    );
    ok(
        &core,
        "t1",
        "CREATE TRIGGER g INSTEAD OF DELETE ON s.jv FOR EACH ROW EXECUTE FUNCTION f_del()",
    );
    ok(
        &core,
        "t1",
        "INSERT INTO s.a (k, v) VALUES (1, 10), (1, 11), (2, 20)",
    );
    ok(
        &core,
        "t1",
        "INSERT INTO s.b (k, w) VALUES (1, 100), (2, 200)",
    );
    assert_eq!(count(&core, "t1", "DELETE FROM s.agg WHERE c = 2"), 1);
    assert_eq!(audit(&core, "del"), vec![1]);
    assert_eq!(count(&core, "t1", "DELETE FROM s.jv WHERE w = 200"), 1);
    assert_eq!(audit(&core, "del"), vec![1, 2]);
}

// トリガ本体の書き込みは呼び出しテナントの行になり、別テナントからは見えない。
#[test]
fn trigger_body_writes_belong_to_the_calling_tenant() {
    let (core, _g) = core_with("instof-owner");
    setup(&core);
    audit_triggers(&core, "t1", "s.nv");
    // 監査表は共有 public 表（行は呼び出しテナントのもの）。別テナントからは 1 行も見えない。
    assert_eq!(count(&core, "t1", "INSERT INTO s.nv (n) VALUES (1)"), 1);
    assert_eq!(audit(&core, "ins"), vec![1]);
    assert!(ints(&core, "t2", "SELECT n FROM audit").is_empty());
}

// INSTEAD OF 経路でもビュー自身への INSERT／UPDATE／DELETE 権限（42501）が効く。
#[test]
fn view_privileges_are_enforced_on_the_instead_of_path() {
    use crate::catalog::role::{RoleAttributes, RoleDef};
    let (core, _g) = core_with("instof-priv");
    let mk = |name: &str| {
        core.storage
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
    let bound = |id: u64, ddl: bool| {
        let mut s = crate::sql::mode::SessionState::default();
        s.bind_role_ids(id, id);
        assert!(s.bind_resolution_tenant("t1"));
        if ddl {
            s.allow_ddl();
        }
        s
    };
    let admin_run = |sql: &str| {
        let mut s = bound(admin, true);
        core.execute_sql_in_session(&ctx("t1"), &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    };
    let alice_code = |sql: &str| {
        let mut s = bound(alice, false);
        match core.execute_sql_in_session(&ctx("t1"), &mut s, sql) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.wire_code().to_string(),
        }
    };
    admin_run("CREATE SCHEMA app");
    admin_run("CREATE TABLE app.tt (n INTEGER)");
    admin_run("CREATE VIEW app.v AS SELECT n FROM app.tt LIMIT 10");
    admin_run(
        "CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    );
    for ev in ["INSERT", "UPDATE", "DELETE"] {
        admin_run(&format!(
            "CREATE TRIGGER g_{ev} INSTEAD OF {ev} ON app.v FOR EACH ROW EXECUTE FUNCTION f()"
        ));
    }
    let ins = "INSERT INTO app.v (n) VALUES (1)";
    let upd = "UPDATE app.v SET n = 2 WHERE n = 1";
    let del = "DELETE FROM app.v WHERE n = 1";
    for sql in [ins, upd, del] {
        assert_eq!(alice_code(sql), "42501", "{sql}");
    }
    admin_run("GRANT USAGE ON SCHEMA app TO alice");
    admin_run("GRANT INSERT ON app.v TO alice");
    assert_eq!(alice_code(ins), "ok");
    // INSERT の権限だけでは UPDATE・DELETE は通らない。
    assert_eq!(alice_code(upd), "42501");
    assert_eq!(alice_code(del), "42501");
    admin_run("GRANT SELECT, UPDATE, DELETE ON app.v TO alice");
    assert_eq!(alice_code(upd), "ok");
    assert_eq!(alice_code(del), "ok");
}
