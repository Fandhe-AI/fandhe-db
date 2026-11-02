//! `COPY` と HTTP（NoSQL）表層の bound 書き込み API の表単位権限検査（SQL-40・NOSQL-2 ポインタ。
//! Issue #1969。設計は `docs/design/role-privilege-model.md` §18、経路一覧は
//! `docs/design/privilege-check-paths.md`）の結合テスト。
//!
//! 現在ロールを束縛したセッション（`SessionState::bind_role_ids`）で、COPY FROM の INSERT・COPY TO の
//! SELECT、CopyDone 後の commit 時の再判定、拡張クエリの Parse・Bind、HTTP 表層の bound 書き込み入口が
//! SQL 表層と同じ判定を受けること、拒否時に行データが変化しないこと、未束縛のセッションが従来と同一に
//! 動くことを固定する。ロールを作る `Storage::create_role` が `pub(crate)` のため crate 内に置く。

use super::{CopyPlan, EngineCore};
use crate::catalog::role::{RoleAttributes, RoleDef};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::{InsertLiteral, WherePredicate};
use crate::sql::mode::SessionState;
use crate::sql::parser::{BoundDelete, BoundInsert};
use crate::sql::transaction::TransactionStatus;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const A: &str = "tenant-a";

struct Fx {
    core: EngineCore,
    _g: CleanupGuard,
    admin: u64,
    alice: u64,
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities(A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn role(storage: &Storage, name: &str) -> u64 {
    storage
        .create_role(
            A,
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

fn fx(label: &str) -> Fx {
    let path = unique_db_path(label);
    let g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    for name in ["ca", "cb"] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("n", ColumnType::BigInt, false),
                ],
            ))
            .expect("table");
    }
    let admin = role(&storage, "admin_r");
    let alice = role(&storage, "alice");
    Fx {
        core: EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        _g: g,
        admin,
        alice,
    }
}

fn bound(id: u64, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    assert!(s.bind_resolution_tenant(A));
    if ddl {
        s.allow_ddl();
    }
    s
}

impl Fx {
    fn admin_run(&self, sql: &str) {
        let mut s = bound(self.admin, true);
        self.core
            .execute_sql_in_session(&ctx(), &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }

    /// 管理者として行数を数える（権限検査の対象外）。
    fn count(&self, table: &str) -> usize {
        let mut s = bound(self.admin, true);
        let out = self
            .core
            .execute_sql_in_session(
                &ctx(),
                &mut s,
                &format!("SELECT id FROM {table} LIMIT 1000"),
            )
            .expect("select");
        match out {
            crate::sql::SqlOutcome::Query(q) => q.rows.len(),
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    /// COPY FROM の begin。拒否なら wire_code を返す。成功なら取り込みセッション。
    fn begin_from(
        &self,
        s: &SessionState,
        sql: &str,
    ) -> Result<crate::sql::copy::CopyInSession, String> {
        match self.core.begin_copy(&ctx(), s, sql) {
            Ok(CopyPlan::From(cs)) => Ok(cs),
            Ok(CopyPlan::To(..)) => panic!("expected COPY FROM plan"),
            Err(e) => Err(e.wire_code().to_string()),
        }
    }

    fn begin_to(&self, s: &SessionState, sql: &str) -> String {
        match self.core.begin_copy(&ctx(), s, sql) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.wire_code().to_string(),
        }
    }
}

fn copy_from(table: &str, op: &str) -> String {
    format!("COPY {table} (id, embedding, n) FROM STDIN USING OPERATION_ID '{op}'")
}

fn finish(mut cs: crate::sql::copy::CopyInSession) -> crate::sql::copy::CopyInBatch {
    cs.feed(b"1\t[1.0, 0.0]\t1\n").expect("feed");
    cs.finish().expect("finish")
}

#[test]
fn copy_from_needs_insert_privilege_before_copy_in_response() {
    let f = fx("copypriv-from");
    let s = bound(f.alice, false);
    assert_eq!(
        f.begin_from(&s, &copy_from("ca", "c1")).err().as_deref(),
        Some("42501")
    );
    f.admin_run("GRANT INSERT ON ca TO alice");
    let cs = f.begin_from(&s, &copy_from("ca", "c1")).expect("granted");
    f.core
        .commit_copy_in(&ctx(), finish(cs))
        .expect("commit with insert privilege");
    assert_eq!(f.count("ca"), 1);
    // 別表には付与が及ばない。
    assert_eq!(
        f.begin_from(&s, &copy_from("cb", "c2")).err().as_deref(),
        Some("42501")
    );
}

#[test]
fn revoke_between_begin_and_commit_is_rejected_without_writing() {
    let f = fx("copypriv-recheck");
    f.admin_run("GRANT INSERT ON ca TO alice");
    let s = bound(f.alice, false);
    let cs = f.begin_from(&s, &copy_from("ca", "r1")).expect("begin");
    let batch = finish(cs);
    f.admin_run("REVOKE INSERT ON ca FROM alice");
    let e = f
        .core
        .commit_copy_in(&ctx(), batch)
        .expect_err("revoked before commit");
    assert_eq!(e.wire_code(), "42501");
    assert_eq!(f.count("ca"), 0);
}

#[test]
fn revoke_in_explicit_txn_fails_the_transaction() {
    let f = fx("copypriv-recheck-txn");
    f.admin_run("GRANT INSERT ON ca TO alice");
    let mut s = bound(f.alice, false);
    let mut txn = f.core.new_session_transaction();
    f.core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let plan = f
        .core
        .begin_copy_in_txn(&ctx(), &s, &mut txn, &copy_from("ca", "rt1"))
        .expect("copy begin");
    let CopyPlan::From(cs) = plan else {
        panic!("expected COPY FROM plan");
    };
    let batch = finish(cs);
    f.admin_run("REVOKE INSERT ON ca FROM alice");
    let e = f
        .core
        .commit_copy_in_txn(&ctx(), &mut txn, batch)
        .expect_err("revoked before commit");
    assert_eq!(e.wire_code(), "42501");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    f.core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(f.count("ca"), 0);
}

#[test]
fn copy_to_needs_select_on_every_referenced_relation() {
    let f = fx("copypriv-to");
    let s = bound(f.alice, false);
    assert_eq!(f.begin_to(&s, "COPY ca TO STDOUT"), "42501");
    assert_eq!(
        f.begin_to(&s, "COPY (SELECT id FROM ca) TO STDOUT"),
        "42501"
    );
    f.admin_run("GRANT SELECT ON ca TO alice");
    assert_eq!(f.begin_to(&s, "COPY ca TO STDOUT"), "ok");
    assert_eq!(f.begin_to(&s, "COPY (SELECT id FROM ca) TO STDOUT"), "ok");
    // 付与は表ごと。ca への付与は cb に及ばない（COPY の括弧形は単一表の走査だけを受け付ける）。
    assert_eq!(f.begin_to(&s, "COPY cb TO STDOUT"), "42501");
    f.admin_run("GRANT SELECT ON cb TO alice");
    assert_eq!(f.begin_to(&s, "COPY cb TO STDOUT"), "ok");
}

#[test]
fn column_level_grant_does_not_satisfy_table_level_copy() {
    let f = fx("copypriv-column");
    let s = bound(f.alice, false);
    f.admin_run("GRANT SELECT (n) ON ca TO alice");
    assert_eq!(f.begin_to(&s, "COPY ca TO STDOUT"), "42501");
    f.admin_run("GRANT INSERT (n) ON ca TO alice");
    assert_eq!(
        f.begin_from(&s, &copy_from("ca", "col1")).err().as_deref(),
        Some("42501")
    );
}

#[test]
fn error_order_is_syntax_then_resolution_then_privilege() {
    let f = fx("copypriv-order");
    let s = bound(f.alice, false);
    assert_eq!(f.begin_to(&s, "COPY ca TO"), "42601");
    assert_eq!(f.begin_to(&s, "COPY nosuch TO STDOUT"), "42P01");
    assert_eq!(
        f.begin_from(&s, &copy_from("nosuch", "o1"))
            .err()
            .as_deref(),
        Some("42P01")
    );
}

#[test]
fn privilege_check_precedes_read_only_transaction_rejection() {
    let f = fx("copypriv-readonly");
    let mut s = bound(f.alice, false);
    let mut txn = f.core.new_session_transaction();
    f.core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, "BEGIN READ ONLY")
        .expect("begin");
    let e = f
        .core
        .begin_copy_in_txn(&ctx(), &s, &mut txn, &copy_from("ca", "ro1"))
        .expect_err("denied");
    assert_eq!(e.wire_code(), "42501");
    f.core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");

    f.admin_run("GRANT INSERT ON ca TO alice");
    f.core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, "BEGIN READ ONLY")
        .expect("begin");
    let e = f
        .core
        .begin_copy_in_txn(&ctx(), &s, &mut txn, &copy_from("ca", "ro2"))
        .expect_err("read only");
    assert_eq!(e.wire_code(), "25006");
}

#[test]
fn temp_table_copy_is_not_affected_by_grants() {
    let f = fx("copypriv-temp");
    let mut s = bound(f.alice, false);
    f.core
        .execute_sql_in_session(
            &ctx(),
            &mut s,
            "CREATE TEMP TABLE tmp1 (id BIGINT PRIMARY KEY, n BIGINT)",
        )
        .expect("temp table");
    let plan = f
        .core
        .begin_copy(&ctx(), &s, "COPY tmp1 (id, n) FROM STDIN")
        .expect("temp COPY FROM needs no grant");
    assert!(matches!(plan, CopyPlan::From(_)));
    assert!(f.core.begin_copy(&ctx(), &s, "COPY tmp1 TO STDOUT").is_ok());
}

#[test]
fn unbound_and_operator_sessions_are_unchanged() {
    let f = fx("copypriv-unbound");
    let s = SessionState::default();
    let cs = f.begin_from(&s, &copy_from("ca", "u1")).expect("unbound");
    f.core.commit_copy_in(&ctx(), finish(cs)).expect("commit");
    assert_eq!(f.begin_to(&s, "COPY ca TO STDOUT"), "ok");
    let op = bound(f.alice, true);
    assert_eq!(f.begin_to(&op, "COPY ca TO STDOUT"), "ok");
    assert!(f.begin_from(&op, &copy_from("cb", "u2")).is_ok());
}

#[test]
fn extended_query_copy_is_checked_at_parse_and_bind() {
    let f = fx("copypriv-extended");
    let s = bound(f.alice, false);
    let sql = "COPY ca TO STDOUT";
    let e = f
        .core
        .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
        .expect_err("denied at parse");
    assert_eq!(e.wire_code(), "42501");
    f.admin_run("GRANT SELECT ON ca TO alice");
    let prepared = f
        .core
        .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
        .expect("parse");
    let txn = f.core.new_session_transaction();
    f.core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect("bind");
    f.admin_run("REVOKE SELECT ON ca FROM alice");
    let e = f
        .core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect_err("denied at bind");
    assert_eq!(e.wire_code(), "42501");
}

// ---------- HTTP（NoSQL）表層の bound 書き込み入口 ----------

fn op(raw: &str) -> OperationId {
    OperationId::parse(raw).expect("operation id")
}

fn bound_insert(id: u64, raw_op: &str) -> BoundInsert {
    BoundInsert {
        table: "ca".to_string(),
        id,
        values: vec![Value::Vector(vec![1.0, 0.0]), Value::BigInt(1)],
        operation_id: Some(op(raw_op)),
        volatile_default_columns: Vec::new(),
        id_omitted: false,
    }
}

fn eq_pred() -> Vec<WherePredicate> {
    vec![WherePredicate::Equality {
        column: "n".to_string(),
        value: "1".to_string(),
    }]
}

fn set_n() -> Vec<(String, InsertLiteral)> {
    vec![("n".to_string(), InsertLiteral::Number("2".to_string()))]
}

#[test]
fn bound_insert_needs_insert_privilege() {
    let f = fx("httppriv-insert");
    let s = bound(f.alice, false);
    let run = |raw_op: &'static str| {
        f.core
            .execute_bound_insert_in_session(&ctx(), &s, "ca", 1, Some(&op(raw_op)), |_| {
                Ok(vec![bound_insert(1, raw_op)])
            })
    };
    assert_eq!(run("h1").expect_err("denied").wire_code(), "42501");
    assert_eq!(f.count("ca"), 0);
    f.admin_run("GRANT INSERT ON ca TO alice");
    run("h1").expect("granted");
    assert_eq!(f.count("ca"), 1);
}

#[test]
fn bound_single_row_update_and_delete_need_select_too() {
    let f = fx("httppriv-single");
    f.admin_run(
        "INSERT INTO ca (id, embedding, n) VALUES (1, '[1.0, 0.0]', 1) USING OPERATION_ID 'seed'",
    );
    let s = bound(f.alice, false);
    let del = BoundDelete {
        table: "ca".to_string(),
        id: 1,
        operation_id: Some(op("hd1")),
    };
    let e = f
        .core
        .execute_bound_delete_in_session(&ctx(), &s, &del)
        .expect_err("denied");
    assert_eq!(e.wire_code(), "42501");
    f.admin_run("GRANT DELETE ON ca TO alice");
    // DELETE だけでは `WHERE id =` の形は通らない（SQL 表層と同じく SELECT も要る）。
    let e = f
        .core
        .execute_bound_delete_in_session(&ctx(), &s, &del)
        .expect_err("select missing");
    assert_eq!(e.wire_code(), "42501");
    let e = f
        .core
        .execute_bound_delete_checked_in_session(&ctx(), &s, &del, |_| Ok(()))
        .expect_err("select missing");
    assert_eq!(e.wire_code(), "42501");
    assert_eq!(f.count("ca"), 1);
    f.admin_run("GRANT SELECT ON ca TO alice");
    f.core
        .execute_bound_delete_in_session(&ctx(), &s, &del)
        .expect("granted");
    assert_eq!(f.count("ca"), 0);
}

#[test]
fn bound_delete_checks_resolution_before_privilege_for_bound_principals() {
    let f = fx("httppriv-delete-order");
    let s = bound(f.alice, false);
    let del = BoundDelete {
        table: "nosuch".to_string(),
        id: 1,
        operation_id: Some(op("hd2")),
    };
    let e = f
        .core
        .execute_bound_delete_in_session(&ctx(), &s, &del)
        .expect_err("undefined table");
    assert_eq!(e.wire_code(), "42P01");
}

#[test]
fn bound_predicate_forms_use_the_same_bits_as_sql() {
    let f = fx("httppriv-predicate");
    let s = bound(f.alice, false);
    let upd = |pred: Vec<WherePredicate>, raw_op: &'static str| {
        f.core.execute_bound_predicate_update_in_session(
            &ctx(),
            &s,
            "ca",
            Some(&op(raw_op)),
            move |_| Ok((set_n(), pred)),
        )
    };
    assert_eq!(
        upd(eq_pred(), "p1").expect_err("denied").wire_code(),
        "42501"
    );
    f.admin_run("GRANT UPDATE ON ca TO alice");
    // 述語形は WHERE を必ず持つので、UPDATE だけでは足りず SELECT も要る（SQL 表層と同じ）。
    assert_eq!(
        upd(eq_pred(), "p3")
            .expect_err("select missing")
            .wire_code(),
        "42501"
    );
    f.admin_run("GRANT SELECT ON ca TO alice");
    upd(eq_pred(), "p3").expect("granted");

    let del = |pred: Vec<WherePredicate>, raw_op: &'static str| {
        f.core.execute_bound_predicate_delete_in_session(
            &ctx(),
            &s,
            "ca",
            Some(&op(raw_op)),
            move |_| Ok(pred),
        )
    };
    assert_eq!(
        del(eq_pred(), "q1").expect_err("denied").wire_code(),
        "42501"
    );
    f.admin_run("GRANT DELETE ON ca TO alice");
    del(eq_pred(), "q1").expect("granted");
}

#[test]
fn bound_partitioned_forms_are_checked() {
    let f = fx("httppriv-partitioned");
    let s = bound(f.alice, false);
    let e = f
        .core
        .execute_bound_partitioned_update_in_session(
            &ctx(),
            &s,
            "ca",
            Some(&op("pu1")),
            None,
            |_| Ok((set_n(), eq_pred())),
        )
        .expect_err("denied");
    assert_eq!(e.wire_code(), "42501");
    let e = f
        .core
        .execute_bound_partitioned_delete_in_session(
            &ctx(),
            &s,
            "ca",
            Some(&op("pd1")),
            None,
            |_| Ok(eq_pred()),
        )
        .expect_err("denied");
    assert_eq!(e.wire_code(), "42501");
    f.admin_run("GRANT UPDATE ON ca TO alice");
    f.admin_run("GRANT DELETE ON ca TO alice");
    f.admin_run("GRANT SELECT ON ca TO alice");
    f.core
        .execute_bound_partitioned_update_in_session(
            &ctx(),
            &s,
            "ca",
            Some(&op("pu1")),
            None,
            |_| Ok((set_n(), eq_pred())),
        )
        .expect("granted");
    f.core
        .execute_bound_partitioned_delete_in_session(
            &ctx(),
            &s,
            "ca",
            Some(&op("pd1")),
            None,
            |_| Ok(eq_pred()),
        )
        .expect("granted");
}

#[test]
fn unbound_session_keeps_bound_writes_unchanged() {
    let f = fx("httppriv-unbound");
    let s = SessionState::default();
    f.core
        .execute_bound_insert_in_session(&ctx(), &s, "ca", 1, Some(&op("w1")), |_| {
            Ok(vec![bound_insert(1, "w1")])
        })
        .expect("unbound insert");
    f.core
        .execute_bound_predicate_delete_in_session(&ctx(), &s, "ca", Some(&op("w2")), |_| {
            Ok(eq_pred())
        })
        .expect("unbound delete");
}

#[test]
fn bound_reads_resolve_table_before_privilege() {
    let f = fx("httppriv-read-order");
    let s = bound(f.alice, false);
    let bind_must_not_run = |_: &crate::catalog::TableSchema,
                             _: &crate::sql::udf_call::UdfRegistry|
     -> Result<
        crate::sql::parser::BoundScan,
        crate::sql::allowlist::SqlSurfaceError,
    > { panic!("bind must not run before the privilege check") };
    // 不在の表は権限の有無に関わらず 42P01。
    let e = f
        .core
        .execute_bound_scan_in_session(&ctx(), &s, "nosuch", bind_must_not_run)
        .expect_err("undefined table");
    assert_eq!(e.wire_code(), "42P01");
    // 在る表で権限なしは 42501（束縛より前に拒否）。
    let e = f
        .core
        .execute_bound_scan_in_session(&ctx(), &s, "ca", bind_must_not_run)
        .expect_err("denied");
    assert_eq!(e.wire_code(), "42501");
}

#[test]
fn recheck_denies_missing_relation() {
    let f = fx("httppriv-recheck-missing");
    let sess = bound(f.alice, false);
    let rc = crate::sql::privilege_gate::write::RelationPrivilegeRecheck::for_session(
        &sess,
        A,
        crate::catalog::privilege::PRIV_INSERT,
    )
    .expect("bound principal");
    let r = f.core.begin_read_txn().expect("txn");
    assert!(rc.check_in_txn(&r, &["nosuch".to_string()]).is_err());
}

/// 明示 Tx の COPY の ACL は `BEGIN` 時点のスナップショットで引く（SELECT・DML ゲートと同じ規則）。
/// `BEGIN` の後に別セッションが付与した権限は、その Tx の COPY には効かない（最新カタログで引くと、
/// スナップショット内の他の読み取りと判定が食い違う）。Tx の外の COPY は最新の ACL で通る。
#[test]
fn explicit_txn_copy_uses_begin_snapshot_acl() {
    let f = fx("copypriv-begin-snapshot");
    let mut s = bound(f.alice, false);
    let mut txn = f.core.new_session_transaction();
    f.core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    f.admin_run("GRANT INSERT ON ca TO alice");
    f.admin_run("GRANT SELECT ON ca TO alice");
    let Err(e) = f
        .core
        .begin_copy_in_txn(&ctx(), &s, &mut txn, &copy_from("ca", "bs1"))
    else {
        panic!("insert granted after BEGIN must not be visible");
    };
    assert_eq!(e.wire_code(), "42501");
    f.core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    // Tx の外では最新の ACL で通る。
    assert!(f.begin_from(&s, &copy_from("ca", "bs2")).is_ok());
}
