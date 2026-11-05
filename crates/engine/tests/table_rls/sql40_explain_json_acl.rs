//! 明示トランザクション内の `EXPLAIN (FORMAT JSON)` の権限検査（Issue #2059・SQL-40・SQL-76 のポインタ）。
//!
//! `acl_snapshot_for` は TEXT の `EXPLAIN` を最新の確定カタログで検査する。`FORMAT JSON` の包みでも
//! 同じ判定（同じ SQLSTATE か同じ成功）になることを固定する。`BEGIN` 後に別セッションが作った表・
//! 付与した権限を、包みの有無で見え方が変わらない（旧スナップショットの ACL で通さない）ことを検証する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ALICE: u64 = 1;
const BOB: u64 = 2;

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("t1", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 明示 Tx 内で `sql` を実行し、成功なら `"ok"`、失敗なら `wire_code` を返す。
fn in_txn(core: &EngineCore, sql: &str, mid_txn: impl FnOnce()) -> String {
    let mut s = SessionState::default();
    s.bind_role_ids(BOB, BOB);
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    mid_txn();
    let r = core.execute_sql_in_txn(&ctx(), &mut s, &mut txn, sql);
    let _ = core.execute_sql_in_txn(&ctx(), &mut s, &mut txn, "ROLLBACK");
    match r {
        Ok(_) => "ok".to_string(),
        Err(e) => e.wire_code().to_string(),
    }
}

fn operator(core: &EngineCore, sql: &str) {
    let mut op = SessionState::default();
    op.bind_role_ids(ALICE, ALICE);
    op.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut op, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// `BEGIN` 後に作られた表・付与された権限・作り直された同名の表に対し、JSON 形式は TEXT と同じ判定になる。
#[test]
fn explain_format_json_matches_text_acl_in_explicit_txn() {
    let path = unique_db_path("sql40_explain_json_acl");
    let _guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    let mut boot = SessionState::default();
    boot.allow_ddl();
    for sql in ["CREATE ROLE alice", "CREATE ROLE bob"] {
        core.execute_sql_in_session(&ctx(), &mut boot, sql)
            .expect("create role");
    }
    for (i, fmt) in ["", "(FORMAT JSON) "].into_iter().enumerate() {
        // 作り直し: bob に付与済みの表を BEGIN 後に DROP して同名で作り直す（付与は引き継がれない）。
        let re = format!("re{i}");
        operator(&core, &format!("CREATE TABLE {re} (a TEXT)"));
        operator(&core, &format!("GRANT SELECT ON {re} TO bob"));
        let recreated = in_txn(&core, &format!("EXPLAIN {fmt}SELECT a FROM {re}"), || {
            operator(&core, &format!("DROP TABLE {re}"));
            operator(&core, &format!("CREATE TABLE {re} (a TEXT)"));
        });
        assert_eq!(recreated, "42501", "recreated table, fmt={fmt:?}");
        // BEGIN 後に作って付与した表は最新カタログで見える。
        let nw = format!("nw{i}");
        let granted = in_txn(&core, &format!("EXPLAIN {fmt}SELECT a FROM {nw}"), || {
            operator(&core, &format!("CREATE TABLE {nw} (a TEXT)"));
            operator(&core, &format!("GRANT SELECT ON {nw} TO bob"));
        });
        assert_eq!(granted, "ok", "granted after begin, fmt={fmt:?}");
        // BEGIN 後に作って付与しない表は権限なしで拒否される。
        let ng = format!("ng{i}");
        let denied = in_txn(&core, &format!("EXPLAIN {fmt}SELECT a FROM {ng}"), || {
            operator(&core, &format!("CREATE TABLE {ng} (a TEXT)"));
        });
        assert_eq!(denied, "42501", "no grant, fmt={fmt:?}");
    }
}
