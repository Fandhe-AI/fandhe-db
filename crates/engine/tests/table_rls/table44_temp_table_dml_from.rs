//! 一時表だけの `UPDATE ... FROM`／`DELETE ... USING`（SQL-53 (b)・TABLE-44・Issue #3292）の結合テスト。
//!
//! 役割: target・source とも一時表の結合 DML が一時コアで実行されること（autocommit・明示 Tx・`RETURNING`）、
//! 同名の永続表へ作用しないこと、台帳の対象外（`USING OPERATION_ID` は黙って捨てず `0A000`）、
//! 永続表・ビューとの混在が `0A000`（fail-closed）のまま副作用ゼロで拒否されることを固定する。
//! ポインタ: `docs/design/update-from-delete-using.md`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "tenant-a";

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities(TENANT, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger),
        guard,
    )
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run_ok(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()))
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx(), s, sql) {
        Ok(o) => format!("ok:{o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// `table` の `(id, n)` を id 昇順で `n` の Debug 文字列にして返す。
fn contents(core: &EngineCore, s: &mut SessionState, table: &str) -> Vec<String> {
    match run_ok(
        core,
        s,
        &format!("SELECT id, n FROM {table} ORDER BY id LIMIT 100"),
    ) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| format!("{:?}", row.cells[1]))
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn affected(o: &SqlOutcome) -> u64 {
    match o {
        SqlOutcome::Update(u) => u.rows_affected,
        SqlOutcome::Delete(d) => d.rows_affected,
        SqlOutcome::Returning(r) => r.rows_affected,
        other => panic!("expected a DML outcome, got {other:?}"),
    }
}

/// 一時表 `a`（更新対象）・`b`（source）。永続表 `a`・`b` を同名で先に作る（作用しないことの確認用）。
fn setup(core: &EngineCore, s: &mut SessionState) {
    for t in ["a", "b"] {
        run_ok(
            core,
            s,
            &format!("CREATE TABLE {t} (id BIGINT PRIMARY KEY, k INTEGER, n INTEGER)"),
        );
        run_ok(
            core,
            s,
            &format!("INSERT INTO {t} (id, k, n) VALUES (1, 1, 100), (2, 2, 200)"),
        );
    }
    for t in ["a", "b"] {
        run_ok(
            core,
            s,
            &format!("CREATE TEMP TABLE {t} (id BIGINT PRIMARY KEY, k INTEGER, n INTEGER)"),
        );
    }
    run_ok(
        core,
        s,
        "INSERT INTO a (id, k, n) VALUES (1, 1, 10), (2, 2, 20), (3, 3, 30)",
    );
    run_ok(
        core,
        s,
        "INSERT INTO b (id, k, n) VALUES (1, 1, 11), (2, 2, 22)",
    );
}

const UPD: &str = "UPDATE a SET n = b.n FROM b WHERE a.k = b.k";
const DEL: &str = "DELETE FROM a USING b WHERE a.k = b.k";

/// 一時表だけの結合 DML が通り、同名の永続表は変わらない。
#[test]
fn temp_only_update_from_and_delete_using_work_and_spare_persistent_tables() {
    let (core, _g) = new_core("t44-dmlfrom-basic");
    let mut s = ddl_session();
    setup(&core, &mut s);
    assert_eq!(affected(&run_ok(&core, &mut s, UPD)), 2);
    assert_eq!(
        contents(&core, &mut s, "a"),
        [
            "SignedInteger(11)",
            "SignedInteger(22)",
            "SignedInteger(30)"
        ]
    );
    assert_eq!(affected(&run_ok(&core, &mut s, DEL)), 2);
    assert_eq!(contents(&core, &mut s, "a"), ["SignedInteger(30)"]);
    // 末尾 `;` つきの単独文も同じ。
    assert_eq!(
        affected(&run_ok(
            &core,
            &mut s,
            "DELETE FROM a USING b WHERE a.k = b.k;"
        )),
        0
    );
    // 一時表を落とすと、同名の永続表が手つかずで現れる。
    run_ok(&core, &mut s, "DROP TABLE a");
    run_ok(&core, &mut s, "DROP TABLE b");
    assert_eq!(
        contents(&core, &mut s, "a"),
        ["SignedInteger(100)", "SignedInteger(200)"]
    );
    assert_eq!(
        contents(&core, &mut s, "b"),
        ["SignedInteger(100)", "SignedInteger(200)"]
    );
}

/// `RETURNING` つきも一時コアで実行できる。
#[test]
fn temp_only_returning_works() {
    let (core, _g) = new_core("t44-dmlfrom-returning");
    let mut s = ddl_session();
    setup(&core, &mut s);
    let out = run_ok(
        &core,
        &mut s,
        "UPDATE a SET n = b.n FROM b WHERE a.k = b.k RETURNING a.id, b.n",
    );
    assert_eq!(affected(&out), 2);
    assert!(matches!(out, SqlOutcome::Returning(_)), "{out:?}");
}

fn tx<'e>(
    core: &'e EngineCore,
    s: &mut SessionState,
    t: &mut SessionTransaction<'e>,
    sql: &str,
) -> Result<SqlOutcome, String> {
    core.execute_sql_in_txn(&ctx(), s, t, sql)
        .map_err(|e| e.wire_code().to_string())
}

/// 明示 Tx の ROLLBACK／COMMIT が一時表の結合 DML に効く。
#[test]
fn temp_only_joined_dml_follows_explicit_transactions() {
    let (core, _g) = new_core("t44-dmlfrom-txn");
    let mut s = ddl_session();
    setup(&core, &mut s);
    let mut t = core.new_session_transaction();
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    assert_eq!(affected(&tx(&core, &mut s, &mut t, UPD).unwrap()), 2);
    assert_eq!(affected(&tx(&core, &mut s, &mut t, DEL).unwrap()), 2);
    tx(&core, &mut s, &mut t, "ROLLBACK").unwrap();
    assert_eq!(
        contents(&core, &mut s, "a"),
        [
            "SignedInteger(10)",
            "SignedInteger(20)",
            "SignedInteger(30)"
        ]
    );
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    assert_eq!(affected(&tx(&core, &mut s, &mut t, DEL).unwrap()), 2);
    tx(&core, &mut s, &mut t, "COMMIT").unwrap();
    assert_eq!(contents(&core, &mut s, "a"), ["SignedInteger(30)"]);
}

/// 台帳は一時表の対象外。`USING OPERATION_ID` は黙って捨てず `0A000`（副作用ゼロ）。
/// 一時表の書き込みは台帳に載らないため、op id の意味（再送の冪等性）を保証できず意図的に拒否する。
/// intentional: docs/design/update-from-delete-using.md
#[test]
fn temp_only_joined_dml_with_operation_id_is_rejected() {
    let (core, _g) = new_core("t44-dmlfrom-opid");
    let mut s = ddl_session();
    setup(&core, &mut s);
    for sql in [
        "UPDATE a SET n = b.n FROM b WHERE a.k = b.k USING OPERATION_ID 'op-1'",
        "DELETE FROM a USING b WHERE a.k = b.k USING OPERATION_ID 'op-2'",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    assert_eq!(
        contents(&core, &mut s, "a"),
        [
            "SignedInteger(10)",
            "SignedInteger(20)",
            "SignedInteger(30)"
        ]
    );
}

/// 永続表・永続ビューとの混在は `0A000` のまま（副作用ゼロ）。
/// 混在形は書き込みが一時側と永続側にまたがるため、別の穴として意図的に拒否する。
/// intentional: docs/design/update-from-delete-using.md
#[test]
fn mixed_temp_and_persistent_joined_dml_is_rejected() {
    let (core, _g) = new_core("t44-dmlfrom-mixed");
    let mut s = ddl_session();
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE p (id BIGINT PRIMARY KEY, k INTEGER, n INTEGER)",
    );
    run_ok(&core, &mut s, "INSERT INTO p (id, k, n) VALUES (1, 1, 100)");
    run_ok(&core, &mut s, "CREATE VIEW pv AS SELECT id, k, n FROM p");
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE a (id BIGINT PRIMARY KEY, k INTEGER, n INTEGER)",
    );
    run_ok(&core, &mut s, "INSERT INTO a (id, k, n) VALUES (1, 1, 10)");
    for sql in [
        "UPDATE a SET n = p.n FROM p WHERE a.k = p.k",
        "DELETE FROM a USING p WHERE a.k = p.k",
        "UPDATE a SET n = pv.n FROM pv WHERE a.k = pv.k",
        "UPDATE p SET n = a.n FROM a WHERE p.k = a.k",
        "DELETE FROM p USING a WHERE p.k = a.k",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    assert_eq!(contents(&core, &mut s, "a"), ["SignedInteger(10)"]);
    assert_eq!(contents(&core, &mut s, "p"), ["SignedInteger(100)"]);
}
