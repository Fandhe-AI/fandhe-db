//! `MERGE ... USING OPERATION_ID` の台帳（再送）の SQL 経由の結合テスト（SQL-79・TABLE-43・TABLE-45・
//! Issue #3357。親 #3349、設計記録 `docs/design/sql-merge.md` の D9〜D11・§13）。
//!
//! 契約: 同じ `operation_id` で同じ内容を再送すると `23505`、別の内容に再利用すると `22023` で、行・トリガ・
//! 採番には触れない。台帳へ記録するのは文全体で 1 エントリ（区間ごとには記録しない）。失敗した文
//! （`21000`・束縛エラー・ROLLBACK）は台帳を消費しない。台帳キーはテナントごとに独立する
//! （3 テナントの越境の網羅は `sql79_merge_cross_tenant.rs`）。
//! 期待値は設計記録の決定から導く（PG17 との突き合わせは `sql79_merge_pg17_parity.rs`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

type Failure = (String, String);

fn new_core_with(label: &str, mode: Option<LedgerMode>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    for (name, col) in [("t", "x"), ("s", "v")] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("k", ColumnType::Text, false),
                    ColumnDef::new(col, ColumnType::Integer, true),
                ],
            ))
            .expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match mode {
        Some(m) => core.with_ledger_mode(m),
        None => core,
    };
    (core, CleanupGuard(path))
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    new_core_with(label, None)
}

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, Failure> {
    core.execute_sql_in_session(&ctx_of(tenant), &mut ddl_session(), sql)
        .map_err(|e| (e.wire_code().to_string(), e.client_message()))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, Failure> {
    run_as(core, "alice", sql)
}

fn ok(core: &EngineCore, sql: &str) {
    run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn merged_as(core: &EngineCore, tenant: &str, sql: &str) -> u64 {
    match run_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Merge(o) => o.rows_affected,
        other => panic!("{sql}: unexpected outcome {other:?}"),
    }
}

fn merged(core: &EngineCore, sql: &str) -> u64 {
    merged_as(core, "alice", sql)
}

fn code(core: &EngineCore, sql: &str) -> String {
    run(core, sql).expect_err(sql).0
}

fn rows_as(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .collect::<Vec<_>>()
            .join(" | "),
        other => panic!("{sql}: unexpected outcome {other:?}"),
    }
}

fn rows(core: &EngineCore, sql: &str) -> String {
    rows_as(core, "alice", sql)
}

/// 末尾 `;` なし・ありの両方を流す補助（psql 流の単独送信）。
fn with_semicolon(sql: &str) -> [String; 2] {
    [sql.to_string(), format!("{sql};")]
}

fn put_t(core: &EngineCore, tenant: &str, id: u64, k: &str, x: i32) {
    run_as(
        core,
        tenant,
        &format!("INSERT INTO t (id, embedding, k, x) VALUES ({id}, '[0.1,0.2]', '{k}', {x})"),
    )
    .expect("seed t");
}

fn put_s(core: &EngineCore, tenant: &str, id: u64, k: &str, v: i32) {
    run_as(
        core,
        tenant,
        &format!("INSERT INTO s (id, embedding, k, v) VALUES ({id}, '[0.1,0.2]', '{k}', {v})"),
    )
    .expect("seed s");
}

/// t = {1:a/10, 2:b/20, 3:c/30}、s = {1:a/100, 3:c/300, 4:d/400}。
fn seed_as(core: &EngineCore, tenant: &str) {
    for (id, k, x) in [(1, "a", 10), (2, "b", 20), (3, "c", 30)] {
        put_t(core, tenant, id, k, x);
    }
    for (id, k, v) in [(1, "a", 100), (3, "c", 300), (4, "d", 400)] {
        put_s(core, tenant, id, k, v);
    }
}

fn seed(core: &EngineCore) {
    seed_as(core, "alice");
}

const T_ALL: &str = "SELECT id, k, x FROM t ORDER BY id LIMIT 100";
const S_ALL: &str = "SELECT id, k, v FROM s ORDER BY id LIMIT 100";

fn snapshot(core: &EngineCore) -> String {
    format!("{}\n{}", rows(core, T_ALL), rows(core, S_ALL))
}

/// UPDATE・DELETE・INSERT の 3 区間を書く MERGE（a を削除・c を更新・d を挿入）。
const M3: &str = "MERGE INTO t USING s ON t.k = s.k \
    WHEN MATCHED AND s.k = 'a' THEN DELETE \
    WHEN MATCHED THEN UPDATE SET x = s.v \
    WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (50, '[0.1,0.2]', s.k, s.v)";

fn with_op(sql: &str, op: &str) -> String {
    format!("{sql} USING OPERATION_ID '{op}'")
}

const T_AFTER_M3: &str =
    "Integer(2):Text(\"b\"):SignedInteger(20) | Integer(3):Text(\"c\"):SignedInteger(300) | \
     Integer(50):Text(\"d\"):SignedInteger(400)";

#[test]
fn an_identical_resend_is_23505_and_changes_nothing() {
    for text in with_semicolon(&with_op(M3, "m-1")) {
        let (core, _g) = new_core("sql79l-resend");
        seed(&core);
        assert_eq!(merged(&core, &text), 3, "{text}");
        assert_eq!(rows(&core, T_ALL), T_AFTER_M3, "{text}");
        let after = snapshot(&core);
        for _ in 0..2 {
            assert_eq!(code(&core, &text), "23505", "{text}");
            assert_eq!(after, snapshot(&core), "{text}");
        }
    }
}

#[test]
fn whitespace_comments_and_returning_do_not_change_the_statement_identity() {
    let (core, _g) = new_core("sql79l-identity");
    seed(&core);
    let first = format!("{M3} RETURNING t.k USING OPERATION_ID 'm-1'");
    ok(&core, &first);
    let after = snapshot(&core);
    let spaced = M3.replace(" WHEN ", "\n  WHEN  ");
    for text in [
        with_op(&spaced, "m-1"),
        with_op(&format!("{M3} /* c */"), "m-1"),
        format!("{M3} RETURNING t.k, t.x USING OPERATION_ID 'm-1';"),
        with_op(M3, "m-1"),
    ] {
        assert_eq!(code(&core, &text), "23505", "{text}");
        assert_eq!(after, snapshot(&core), "{text}");
    }
}

#[test]
fn a_different_statement_with_the_same_operation_id_is_22023() {
    let (core, _g) = new_core("sql79l-mismatch");
    seed(&core);
    ok(&core, &with_op(M3, "m-1"));
    let after = snapshot(&core);
    let variants = [
        // リテラルの違い。
        with_op(&M3.replace("'a'", "'b'"), "m-1"),
        // WHEN の追加。
        with_op(
            &format!("{M3} WHEN NOT MATCHED BY SOURCE THEN DELETE"),
            "m-1",
        ),
        // 単独の DML・結合形 DML との取り違え。
        with_op("DELETE FROM t WHERE k = 'zz'", "m-1"),
        with_op("UPDATE t SET x = 1 WHERE k = 'zz'", "m-1"),
        with_op(
            "UPDATE t SET x = s.v FROM s WHERE t.k = s.k AND s.k = 'zz'",
            "m-1",
        ),
    ];
    for text in variants {
        assert_eq!(code(&core, &text), "22023", "{text}");
        assert_eq!(after, snapshot(&core), "{text}");
    }
}

#[test]
fn the_same_statement_is_still_23505_after_the_data_changed() {
    let (core, _g) = new_core("sql79l-data-change");
    seed(&core);
    ok(&core, &with_op(M3, "m-1"));
    put_s(&core, "alice", 9, "z", 1);
    ok(&core, "UPDATE t SET x = 7 WHERE k = 'b'");
    let after = snapshot(&core);
    assert_eq!(code(&core, &with_op(M3, "m-1")), "23505");
    assert_eq!(after, snapshot(&core));
}

#[test]
fn a_merge_that_matches_nothing_is_recorded_too() {
    let (core, _g) = new_core("sql79l-empty");
    seed(&core);
    let sql = with_op(
        "MERGE INTO t USING s ON t.k = s.k AND s.k = 'nope' WHEN MATCHED THEN DELETE",
        "m-0",
    );
    assert_eq!(merged(&core, &sql), 0);
    let after = snapshot(&core);
    assert_eq!(code(&core, &sql), "23505");
    assert_eq!(after, snapshot(&core));
}

#[test]
fn failed_statements_do_not_consume_the_ledger() {
    let (core, _g) = new_core("sql79l-failures");
    seed(&core);
    // 21000: 元に同じキーが 2 行ある。
    put_s(&core, "alice", 8, "c", 301);
    let before = snapshot(&core);
    let upd = with_op(
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v",
        "m-f",
    );
    assert_eq!(code(&core, &upd), "21000");
    // 束縛エラー。
    let bad = with_op(
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET nope = 1",
        "m-f",
    );
    assert_eq!(code(&core, &bad), "42703");
    assert_eq!(before, snapshot(&core));
    // 原因を除いて同じ op id で再送すると成功する（台帳を消費していない）。
    ok(&core, "DELETE FROM s WHERE v = 301");
    assert_eq!(merged(&core, &upd), 2);
    assert_eq!(code(&core, &upd), "23505");
}

#[test]
fn ledgered_mode_records_three_segments_once_and_still_requires_the_id() {
    let (core, _g) = new_core("sql79l-ledgered");
    seed(&core);
    let core = core.with_ledger_mode(LedgerMode::Ledgered);
    let sql = with_op(M3, "m-1");
    assert_eq!(merged(&core, &sql), 3);
    assert_eq!(code(&core, &sql), "23505");
    assert_eq!(code(&core, M3), "23502");
    assert_eq!(rows(&core, T_ALL), T_AFTER_M3);
}

#[test]
fn compare_only_mode_runs_with_an_id_and_does_not_replay() {
    let (core, _g) = new_core_with("sql79l-compare", Some(LedgerMode::CompareOnlyWithoutLedger));
    seed(&core);
    let sql = with_op(
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DO NOTHING",
        "m-1",
    );
    assert_eq!(merged(&core, &sql), 0);
    assert_eq!(merged(&core, &sql), 0);
}

#[test]
fn a_resend_fires_no_triggers_and_advances_no_sequence() {
    let (core, _g) = new_core("sql79l-sideeffects");
    ok(&core, "CREATE SCHEMA s");
    ok(&core, "CREATE SEQUENCE s.sq");
    ok(&core, "CREATE TABLE audit (lv TEXT)");
    ok(
        &core,
        "CREATE TABLE s.pt (k TEXT, x INTEGER, n BIGINT DEFAULT nextval('s.sq'), g INTEGER GENERATED ALWAYS AS (x * 2) STORED)",
    );
    ok(&core, "CREATE TABLE s.ps (k TEXT, v INTEGER)");
    ok(&core, "INSERT INTO s.pt (k, x) VALUES ('a', 10)");
    ok(&core, "INSERT INTO s.ps (k, v) VALUES ('a', 1), ('b', 2)");
    for (name, timing_level) in [
        ("bs", "BEFORE INSERT ON s.pt FOR EACH STATEMENT"),
        ("as", "AFTER UPDATE ON s.pt FOR EACH STATEMENT"),
        ("ar", "AFTER INSERT ON s.pt FOR EACH ROW"),
    ] {
        ok(
            &core,
            &format!(
                "CREATE FUNCTION f_{name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit (lv) VALUES ('{name}'); RETURN NULL; END $$"
            ),
        );
        ok(
            &core,
            &format!("CREATE TRIGGER g_{name} {timing_level} EXECUTE FUNCTION f_{name}()"),
        );
    }
    let sql = "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k \
        WHEN MATCHED THEN UPDATE SET x = e.v \
        WHEN NOT MATCHED THEN INSERT (k, x) VALUES (e.k, e.v) USING OPERATION_ID 'm-1'";
    assert_eq!(merged(&core, sql), 2);
    let audit_sql = "SELECT lv FROM audit ORDER BY lv LIMIT 100";
    let table_sql = "SELECT k, x, n, g FROM s.pt ORDER BY k LIMIT 100";
    let audit = rows(&core, audit_sql);
    let table = rows(&core, table_sql);
    assert!(audit.contains("bs") && audit.contains("as"), "{audit}");
    for text in with_semicolon(sql) {
        assert_eq!(code(&core, &text), "23505", "{text}");
        assert_eq!(rows(&core, audit_sql), audit, "{text}");
        assert_eq!(rows(&core, table_sql), table, "{text}");
    }
    // 採番は再送で進まない: 次の INSERT の値は、これまでに配った値の次になる。
    ok(&core, "INSERT INTO s.pt (k, x) VALUES ('z', 1)");
    assert_eq!(
        rows(&core, "SELECT n FROM s.pt WHERE k = 'z' LIMIT 1"),
        "SignedInteger(3)"
    );
}

#[test]
fn view_targets_use_their_own_ledger_domain() {
    let (core, _g) = new_core("sql79l-view");
    seed(&core);
    ok(&core, "CREATE VIEW tv AS SELECT k, x FROM t WHERE x <> 20");
    let via_view = with_op(
        "MERGE INTO tv USING s ON tv.k = s.k WHEN MATCHED THEN DELETE",
        "m-v",
    );
    assert_eq!(merged(&core, &via_view), 2);
    let after = snapshot(&core);
    assert_eq!(code(&core, &via_view), "23505");
    // 同じ op id を基底表への同じ形の MERGE に使うと別ドメイン（`22023`）。
    let direct = with_op(
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
        "m-v",
    );
    assert_eq!(code(&core, &direct), "22023");
    assert_eq!(after, snapshot(&core));
}

fn in_txn(core: &EngineCore, stmts: &[&str]) -> Vec<Result<SqlOutcome, Failure>> {
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    stmts
        .iter()
        .map(|sql| {
            core.execute_sql_in_txn(&ctx_of("alice"), &mut session, &mut txn, sql)
                .map_err(|e| (e.wire_code().to_string(), e.client_message()))
        })
        .collect()
}

#[test]
fn explicit_transactions_follow_commit_and_rollback() {
    let (core, _g) = new_core("sql79l-txn");
    seed(&core);
    let before = snapshot(&core);
    let sql = with_op(M3, "m-t");
    // ROLLBACK 後は台帳が残らず、再送は成功する。
    let out = in_txn(&core, &["BEGIN", &sql, "ROLLBACK"]);
    assert!(out.iter().all(|r| r.is_ok()), "{out:?}");
    assert_eq!(before, snapshot(&core));
    // 同じ Tx 内で同じ op id の再利用は拒否される。
    let out = in_txn(
        &core,
        &[
            "BEGIN",
            &sql,
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DO NOTHING USING OPERATION_ID 'm-t'",
            "ROLLBACK",
        ],
    );
    assert!(out[1].is_ok(), "{out:?}");
    assert!(out[2].is_err(), "{out:?}");
    assert_eq!(before, snapshot(&core));
    // COMMIT 後の再送は `23505`。
    let out = in_txn(&core, &["BEGIN", &sql, "COMMIT"]);
    assert!(out.iter().all(|r| r.is_ok()), "{out:?}");
    let after = snapshot(&core);
    assert_eq!(code(&core, &sql), "23505");
    assert_eq!(after, snapshot(&core));
}

#[test]
fn a_read_only_transaction_rejects_the_statement_without_consuming_the_ledger() {
    let (core, _g) = new_core("sql79l-ro");
    seed(&core);
    let sql = with_op(M3, "m-t");
    let out = in_txn(&core, &["BEGIN READ ONLY", &sql]);
    assert_eq!(out[1].as_ref().expect_err("read only").0, "25006");
    assert_eq!(merged(&core, &sql), 3);
}

fn exec_bound(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    values: &[&str],
) -> Result<SqlOutcome, String> {
    let prepared = core
        .parse_sql_prepared(sql)
        .map_err(|e| e.wire_code().to_string())?;
    let vals: Vec<Option<Vec<u8>>> = values.iter().map(|v| Some(v.as_bytes().to_vec())).collect();
    let parsed = core
        .bind_prepared(&prepared, &vals)
        .map_err(|e| e.wire_code().to_string())?;
    core.execute_parsed_in_session(&ctx_of(tenant), &mut SessionState::default(), &parsed)
        .map_err(|e| e.wire_code().to_string())
}

#[test]
fn extended_query_parameters_feed_the_ledger() {
    let (core, _g) = new_core("sql79l-params");
    seed(&core);
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = $2 USING OPERATION_ID $1";
    // Parse は従来どおり成功する。
    core.parse_sql_prepared(sql).expect("parse");
    match exec_bound(&core, "alice", sql, &["p-1", "5"]) {
        Ok(SqlOutcome::Merge(o)) => assert_eq!(o.rows_affected, 2),
        other => panic!("{other:?}"),
    }
    let after = snapshot(&core);
    assert_eq!(
        exec_bound(&core, "alice", sql, &["p-1", "5"]).expect_err("replay"),
        "23505"
    );
    assert_eq!(
        exec_bound(&core, "alice", sql, &["p-1", "6"]).expect_err("mismatch"),
        "22023"
    );
    assert_eq!(after, snapshot(&core));
}

#[test]
fn ledger_keys_are_scoped_to_the_tenant() {
    let (core, _g) = new_core("sql79l-tenant");
    seed(&core);
    seed_as(&core, "bob");
    let sql = with_op(M3, "m-1");
    assert_eq!(merged_as(&core, "alice", &sql), 3);
    // 別テナントの同じ op id は独立に成功する。
    assert_eq!(merged_as(&core, "bob", &sql), 3);
    // どちらの再送も、自分の台帳だけで `23505` になる。
    for tenant in ["alice", "bob"] {
        let (c, m) = run_as(&core, tenant, &sql).expect_err("replay");
        assert_eq!(c, "23505");
        assert!(!m.contains("alice") && !m.contains("bob"), "{m}");
    }
    // 内容違いのエラーは、どちらのテナントでも同じコード・文言になる。
    let other = with_op(&M3.replace("'a'", "'b'"), "m-1");
    let r1 = run_as(&core, "alice", &other).expect_err("mismatch");
    let r2 = run_as(&core, "bob", &other).expect_err("mismatch");
    assert_eq!(r1, r2);
}

#[test]
fn unsupported_shapes_with_an_operation_id_keep_their_sqlstate_and_do_not_consume_the_ledger() {
    let (core, _g) = new_core("sql79l-failclosed");
    seed(&core);
    let before = snapshot(&core);
    let cte = with_op(
        "WITH m AS (MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING t.k) SELECT * FROM m",
        "m-x",
    );
    assert_eq!(code(&core, &cte), "0A000");
    assert_eq!(before, snapshot(&core));
    // 同じ op id が別の（正常な）MERGE に使える。
    let plain = with_op(
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
        "m-x",
    );
    assert_eq!(merged(&core, &plain), 2);
}
