//! `operation_id` の任意（既定）／必須（`--require-operation-id`）切替の行列回帰テスト
//! （Issue #1705、対象ビヘイビア: RECOVER-1。ポインタ: `docs/spec/04-behavior/recovery.md`
//! RECOVER-1・`docs/design/operation-id-optional.md`）。
//!
//! 「`operation_id` 省略／指定」×「既定（`LedgerMode::Optional`）／必須（`LedgerMode::Ledgered`。
//! wire-server の `--require-operation-id` が解決する値）」の全組み合わせを 1 本の行列で固定し、
//! SQL・SET 等のセッション操作からモードを切り替えられないことを両方向で固定する。
//! 実装は変えない。個別機能の網羅は `recovery_optional_op_id.rs`・`recovery_required_op_id.rs`・
//! `copy_from.rs`・`sql_insert_batch_public_api.rs` が担い、本ファイルは「モード差」だけを見る。
//! wire 越しの同種テストは wire-server の `tests/wire_require_operation_id_matrix.rs`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::{LastOperationLookup, LedgerLookup};
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 行列のモード軸。`None` が既定（任意）、`Ledgered` が `--require-operation-id` 相当。
const MODES: [(&str, Option<LedgerMode>); 2] =
    [("default", None), ("required", Some(LedgerMode::Ledgered))];

fn new_core(label: &str, mode: Option<LedgerMode>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(mode) = mode {
        core = core.with_ledger_mode(mode);
    }
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("sys"),
        &mut session,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, n BIGINT, u TEXT)",
    )
    .expect("create table");
    (core, CleanupGuard(path))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("t"), session, sql)
}

fn ok(core: &EngineCore, sql: &str) -> SqlOutcome {
    run(core, &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn code(core: &EngineCore, sql: &str) -> String {
    run(core, &mut SessionState::default(), sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

fn rows(core: &EngineCore) -> usize {
    core.execute_sql(&ctx("t"), "SELECT id FROM docs LIMIT 1000")
        .expect("scan")
        .rows
        .len()
}

fn op(id: &str) -> OperationId {
    OperationId::parse(id).expect("valid operation_id")
}

/// 省略形の書き込み文（句なし／明示 NULL）。seed 行（id=1,2）が前提。
const OMITTED_WRITES: [&str; 10] = [
    "INSERT INTO docs (id, n, u) VALUES (9, 9, 'z')",
    "INSERT INTO docs (id, n, u) VALUES (9, 9, 'z'), (10, 10, 'y')",
    "INSERT INTO docs (id, n, u) VALUES (9, 9, 'z') ON CONFLICT (id) DO NOTHING",
    "INSERT INTO docs (id, n, u) VALUES (1, 1, 'q') ON CONFLICT (id) DO UPDATE SET u = 'q'",
    "UPDATE docs SET u = 'x' WHERE id = 1",
    "UPDATE docs SET u = 'x' WHERE n > 0",
    "DELETE FROM docs WHERE id = 1",
    "DELETE FROM docs WHERE n > 0",
    "TRUNCATE TABLE docs",
    "INSERT INTO docs (id, n, u) VALUES (9, 9, 'z') USING OPERATION_ID NULL",
];

fn seed(core: &EngineCore) {
    ok(
        core,
        "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a'), (2, 2, 'b') USING OPERATION_ID 'seed'",
    );
}

#[test]
fn omitted_operation_id_matrix_follows_mode() {
    for (label, mode) in MODES {
        for sql in OMITTED_WRITES {
            let (core, _g) = new_core(&format!("mx-omit-{label}"), mode);
            seed(&core);
            let before = rows(&core);
            let r = run(&core, &mut SessionState::default(), sql);
            if mode.is_none() {
                r.unwrap_or_else(|e| panic!("{label}: {sql} must succeed, got {e:?}"));
                // 省略書き込みは台帳を進めない（直前の指定付き id のまま）。
                assert_eq!(
                    core.last_operation_id(&ctx("t"), "docs").expect("last"),
                    LastOperationLookup::Committed(op("seed")),
                    "{label}: {sql}"
                );
            } else {
                let e = r.map(|_| ()).expect_err("must be rejected");
                assert_eq!(e.wire_code(), "23502", "{label}: {sql}");
                assert_eq!(rows(&core), before, "{label}: {sql} must not change rows");
            }
        }
    }
}

#[test]
fn explicit_operation_id_contract_is_identical_in_both_modes() {
    let mut observed = Vec::new();
    for (label, mode) in MODES {
        let (core, _g) = new_core(&format!("mx-spec-{label}"), mode);
        let sql = "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a') USING OPERATION_ID 'op-1'";
        ok(&core, sql);
        let resend = code(&core, sql);
        let mismatch = code(
            &core,
            "INSERT INTO docs (id, n, u) VALUES (1, 1, 'different') USING OPERATION_ID 'op-1'",
        );
        let recorded = core
            .operation_recorded(&ctx("t"), "docs", &op("op-1"))
            .expect("lookup");
        assert_eq!(recorded, LedgerLookup::Recorded, "{label}");
        assert_eq!((resend.as_str(), mismatch.as_str()), ("23505", "22023"));
        observed.push((resend, mismatch, rows(&core)));
    }
    assert_eq!(observed[0], observed[1], "モード間で指定時の結果が一致する");
}

#[test]
fn explicit_update_delete_are_recorded_in_both_modes() {
    for (label, mode) in MODES {
        let (core, _g) = new_core(&format!("mx-spec-dml-{label}"), mode);
        seed(&core);
        ok(
            &core,
            "UPDATE docs SET u = 'x' WHERE id = 1 USING OPERATION_ID 'u1'",
        );
        ok(
            &core,
            "DELETE FROM docs WHERE id = 2 USING OPERATION_ID 'd1'",
        );
        assert_eq!(rows(&core), 1, "{label}");
        for id in ["u1", "d1"] {
            assert_eq!(
                core.operation_recorded(&ctx("t"), "docs", &op(id))
                    .expect("lookup"),
                LedgerLookup::Recorded,
                "{label}: {id}"
            );
        }
    }
}

#[test]
fn prepared_specified_binding_is_recorded_in_both_modes() {
    // `USING OPERATION_ID $1` に値を束縛した場合は両モードで同じ台帳契約になる。
    // NULL 束縛は bind 段で拒否される現状の挙動があるが、任意化との整合は別途追跡する
    // （本テストでは固定しない）。
    for (label, mode) in MODES {
        let (core, _g) = new_core(&format!("mx-prep-{label}"), mode);
        let p = core
            .parse_sql_prepared(
                "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a') USING OPERATION_ID $1",
            )
            .expect("parse");
        let parsed = core
            .bind_prepared(&p, &[Some(b"p1".to_vec())])
            .expect("bind value");
        core.execute_parsed_in_session(&ctx("t"), &mut SessionState::default(), &parsed)
            .expect("specified succeeds");
        assert_eq!(
            core.operation_recorded(&ctx("t"), "docs", &op("p1"))
                .expect("lookup"),
            LedgerLookup::Recorded,
            "{label}"
        );
        let e = core
            .execute_parsed_in_session(&ctx("t"), &mut SessionState::default(), &parsed)
            .map(|_| ())
            .expect_err("resend");
        assert_eq!(e.wire_code(), "23505", "{label}");
    }
}

#[test]
fn partitioned_dml_requires_operation_id_in_both_modes_including_bound_entries() {
    for (label, mode) in MODES {
        let (core, _g) = new_core(&format!("mx-part-{label}"), mode);
        seed(&core);
        for sql in [
            "DELETE FROM docs WHERE n > 0 PARTITIONED",
            "DELETE FROM docs WHERE n > 0 USING OPERATION_ID NULL PARTITIONED",
            "UPDATE docs SET u = 'x' WHERE n > 0 PARTITIONED",
            "UPDATE docs SET u = 'x' WHERE n > 0 USING OPERATION_ID NULL PARTITIONED",
            "EXPLAIN DELETE FROM docs WHERE n > 0 USING OPERATION_ID NULL PARTITIONED",
        ] {
            assert_eq!(code(&core, sql), "23502", "{label}: {sql}");
        }
        let c = ctx("t");
        let e = core
            .execute_bound_partitioned_delete_in_session(&c, "docs", None, None, |_s| {
                panic!("bind must not run before the operation_id gate")
            })
            .expect_err("rejected");
        assert_eq!(e.wire_code(), "23502", "{label}");
        let e = core
            .execute_bound_partitioned_update_in_session(&c, "docs", None, None, |_s| {
                panic!("bind must not run before the operation_id gate")
            })
            .expect_err("rejected");
        assert_eq!(e.wire_code(), "23502", "{label}");
        assert_eq!(rows(&core), 2, "{label}: rows untouched");
    }
}

#[test]
fn row_apis_follow_mode_without_operation_id() {
    use engine::storage::RowInput;
    for (label, mode) in MODES {
        let (core, _g) = new_core(&format!("mx-row-{label}"), mode);
        let c = ctx("t");
        let row = RowInput {
            tenant_id: "t",
            visibility: Visibility::Public,
            embedding: &[0.1, 0.2, 0.3],
            metadata: &[],
        };
        let results = [
            core.insert_row(&c, "no_such_table", 1, &row, None)
                .map(|_| ()),
            core.update_row(&c, "no_such_table", 1, &row, None)
                .map(|_| ()),
            core.delete_row(&c, "no_such_table", 1, None).map(|_| ()),
        ];
        for r in results {
            let e = r.expect_err("table does not exist, so every path errors");
            if mode.is_some() {
                // 必須: ストレージ到達前に 23502。
                assert_eq!(e.wire_code(), "23502", "{label}");
            } else {
                // 任意: 台帳ゲートは通過し、後段（カタログ）の別コードで失敗する。
                assert_ne!(e.wire_code(), "23502", "{label}");
            }
        }
    }
}

/// SQL・セッション操作からモードを切り替える試行。いずれも拒否され、モードは変わらない。
const SWITCH_ATTEMPTS: [&str; 10] = [
    "SET require_operation_id = off",
    "SET require_operation_id = on",
    "SET require_operation_id TO true",
    "SET ledger_mode = 'optional'",
    "SET ledger_mode = 'ledgered'",
    "SET fandhe.require_operation_id = 'off'",
    "SET SESSION require_operation_id = off",
    "RESET require_operation_id",
    "RESET ALL",
    "ALTER SYSTEM SET require_operation_id = on",
];

#[test]
fn mode_cannot_be_switched_from_sql_in_either_direction() {
    for (label, mode) in MODES {
        let (core, _g) = new_core(&format!("mx-switch-{label}"), mode);
        let mut session = SessionState::default();
        let mut next_id = 100;
        for attempt in SWITCH_ATTEMPTS {
            let e = run(&core, &mut session, attempt)
                .map(|_| ())
                .expect_err(attempt);
            // 観測値を固定する（初回実行で確定した SQLSTATE をピン留めする）。
            assert_eq!(e.wire_code(), EXPECTED_SWITCH_CODE, "{label}: {attempt}");
            // 失敗時にテーブル名・テナント名を含まない。
            let msg = e.to_string();
            assert!(!msg.contains("docs") && !msg.contains("tenant"), "{msg}");
            // 同一セッション・新規セッションの双方でモードは不変。
            for fresh in [false, true] {
                next_id += 1;
                let sql = format!("INSERT INTO docs (id, n, u) VALUES ({next_id}, 1, 'a')");
                let r = if fresh {
                    run(&core, &mut SessionState::default(), &sql)
                } else {
                    run(&core, &mut session, &sql)
                };
                if mode.is_none() {
                    r.unwrap_or_else(|e| {
                        panic!("{label}: after {attempt}, default must still accept: {e:?}")
                    });
                } else {
                    assert_eq!(
                        r.map(|_| ()).unwrap_err().wire_code(),
                        "23502",
                        "{label}: after {attempt}, required must still reject"
                    );
                }
            }
        }
    }
}

/// `SET`／`RESET`／`ALTER SYSTEM` の未対応・不許可は構文面で拒否される（#1704 の t3 と同じ値）。
const EXPECTED_SWITCH_CODE: &str = "42601";

#[test]
fn set_local_inside_transaction_does_not_switch_mode() {
    for (label, mode) in MODES {
        let (core, _g) = new_core(&format!("mx-setlocal-{label}"), mode);
        let caller = ctx("t");
        let mut txn = core.new_session_transaction();
        let mut s = SessionState::default();
        core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let r = core.execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            "SET LOCAL require_operation_id = off",
        );
        assert!(r.is_err(), "{label}: SET LOCAL must be rejected");
        let _ = core.execute_sql_in_txn(&caller, &mut s, &mut txn, "ROLLBACK");
        let r = run(
            &core,
            &mut SessionState::default(),
            "INSERT INTO docs (id, n, u) VALUES (1, 1, 'a')",
        );
        assert_eq!(r.is_ok(), mode.is_none(), "{label}");
    }
}
