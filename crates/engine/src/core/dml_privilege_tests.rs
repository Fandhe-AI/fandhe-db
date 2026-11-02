//! 書き込み系の文の表単位権限検査（SQL-40・RLS-12・SQL-19・SQL-22 ポインタ。Issue #1968。設計は
//! `docs/design/role-privilege-model.md` §13）の結合テスト。
//!
//! 現在ロールを束縛したセッション（`SessionState::bind_role_ids`）で、`GRANT` で付与した表単位の
//! INSERT／UPDATE／DELETE／TRUNCATE が各文種（UPSERT・RETURNING・INSERT ... SELECT・UPDATE ... FROM・
//! ビュー DML・データ変更 CTE・分割実行・TRUNCATE CASCADE の連鎖先・明示 Tx・拡張クエリ）に効くこと、
//! 拒否時に行データと `operation_id` 台帳が変化しないこと、未束縛のセッションが従来と同一に書けることを
//! 固定する。ロールを作る `Storage::create_role` が `pub(crate)` のため crate 内に置く。

use super::EngineCore;
use crate::catalog::role::{RoleAttributes, RoleDef};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
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

fn code(r: Result<crate::sql::SqlOutcome, SqlSurfaceError>) -> String {
    match r {
        Ok(_) => "ok".to_string(),
        Err(e) => e.wire_code().to_string(),
    }
}

impl Fx {
    fn admin_run(&self, sql: &str) {
        let mut s = bound(self.admin, true);
        self.core
            .execute_sql_in_session(&ctx(), &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }

    fn alice_run(&self, sql: &str) -> String {
        let mut s = bound(self.alice, false);
        code(self.core.execute_sql_in_session(&ctx(), &mut s, sql))
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
}

fn ins(table: &str, id: i64, op: &str) -> String {
    format!(
        "INSERT INTO {table} (id, embedding, n) VALUES ({id}, '[1.0, 0.0]', 1) \
         USING OPERATION_ID '{op}'"
    )
}

#[test]
fn unbound_session_writes_without_any_grant() {
    let f = fx("dmlpriv-unbound");
    let mut s = SessionState::default();
    for sql in [
        ins("ca", 1, "u1"),
        "UPDATE ca SET n = 2 WHERE n > 0 USING OPERATION_ID 'u2'".to_string(),
        "DELETE FROM ca WHERE n > 0 USING OPERATION_ID 'u3'".to_string(),
        "TRUNCATE TABLE ca USING OPERATION_ID 'u4'".to_string(),
    ] {
        assert_eq!(
            code(f.core.execute_sql_in_session(&ctx(), &mut s, &sql)),
            "ok",
            "{sql}"
        );
    }
}

#[test]
fn insert_needs_insert_and_returning_needs_select() {
    let f = fx("dmlpriv-insert");
    assert_eq!(f.alice_run(&ins("ca", 1, "i1")), "42501");
    // 拒否した operation_id は台帳に残らない（付与後の同じ id の再送が新規として成功する）。
    assert_eq!(f.count("ca"), 0);
    f.admin_run("GRANT INSERT ON ca TO alice");
    assert_eq!(f.alice_run(&ins("ca", 1, "i1")), "ok");
    assert_eq!(f.alice_run(&ins("cb", 1, "i2")), "42501");
    let ret = "INSERT INTO ca (id, embedding, n) VALUES (2, '[1.0, 0.0]', 1) RETURNING id \
               USING OPERATION_ID 'i3'";
    assert_eq!(f.alice_run(ret), "42501");
    f.admin_run("GRANT SELECT ON ca TO alice");
    assert_eq!(f.alice_run(ret), "ok");
    assert_eq!(f.count("ca"), 2);
}

#[test]
fn upsert_do_update_needs_update_and_select() {
    let f = fx("dmlpriv-upsert");
    f.admin_run(&ins("ca", 1, "seed"));
    f.admin_run("GRANT INSERT ON ca TO alice");
    let upsert = "INSERT INTO ca (id, embedding, n) VALUES (1, '[1.0, 0.0]', 2) \
                  ON CONFLICT (id) DO UPDATE SET n = 9 USING OPERATION_ID 'up1'";
    assert_eq!(f.alice_run(upsert), "42501");
    f.admin_run("GRANT UPDATE ON ca TO alice");
    assert_eq!(f.alice_run(upsert), "42501");
    f.admin_run("GRANT SELECT ON ca TO alice");
    assert_eq!(f.alice_run(upsert), "ok");
}

#[test]
fn update_and_delete_need_their_privilege_and_select_for_where() {
    let f = fx("dmlpriv-upd-del");
    f.admin_run(&ins("ca", 1, "seed"));
    let upd = "UPDATE ca SET n = 5 WHERE n > 0 USING OPERATION_ID 'x1'";
    let upd_all = "UPDATE ca SET n = 5 USING OPERATION_ID 'x2'";
    assert_eq!(f.alice_run(upd), "42501");
    f.admin_run("GRANT UPDATE ON ca TO alice");
    // WHERE なし・リテラル SET は列を読まないので UPDATE だけで通る。WHERE 付きは SELECT も要る。
    assert_eq!(f.alice_run(upd_all), "ok");
    assert_eq!(f.alice_run(upd), "42501");
    f.admin_run("GRANT SELECT ON ca TO alice");
    assert_eq!(f.alice_run(upd), "ok");

    let del = "DELETE FROM ca WHERE n > 0 USING OPERATION_ID 'd1'";
    assert_eq!(f.alice_run(del), "42501");
    f.admin_run("GRANT DELETE ON ca TO alice");
    assert_eq!(f.alice_run(del), "ok");
    assert_eq!(f.count("ca"), 0);
}

#[test]
fn denied_statement_leaves_rows_and_ledger_untouched() {
    let f = fx("dmlpriv-ledger");
    f.admin_run(&ins("ca", 1, "seed"));
    let del = "DELETE FROM ca WHERE n > 0 USING OPERATION_ID 'dl1'";
    assert_eq!(f.alice_run(del), "42501");
    assert_eq!(f.count("ca"), 1);
    f.admin_run("GRANT DELETE ON ca TO alice");
    f.admin_run("GRANT SELECT ON ca TO alice");
    // 同じ operation_id が新規として成功する（拒否で台帳に記録されていない）。
    assert_eq!(f.alice_run(del), "ok");
    assert_eq!(f.count("ca"), 0);
    // 剥奪後の再送は、重複の成功応答ではなく `42501`（台帳照合より前に拒否）。
    f.admin_run("REVOKE DELETE ON ca FROM alice");
    assert_eq!(f.alice_run(del), "42501");
}

#[test]
fn insert_select_checks_target_and_source() {
    let f = fx("dmlpriv-insel");
    f.admin_run(&ins("cb", 1, "seed"));
    let sql = "INSERT INTO ca (id, embedding, n) SELECT id, embedding, n FROM cb \
               USING OPERATION_ID 'is1'";
    f.admin_run("GRANT INSERT ON ca TO alice");
    assert_eq!(f.alice_run(sql), "42501");
    f.admin_run("GRANT SELECT ON cb TO alice");
    assert_eq!(f.alice_run(sql), "ok");
    assert_eq!(f.count("ca"), 1);
}

#[test]
fn truncate_checks_every_listed_table_and_cascade_closure() {
    let f = fx("dmlpriv-trunc");
    assert_eq!(
        f.alice_run("TRUNCATE TABLE ca USING OPERATION_ID 't1'"),
        "42501"
    );
    f.admin_run("GRANT TRUNCATE ON ca TO alice");
    assert_eq!(
        f.alice_run("TRUNCATE TABLE ca, cb USING OPERATION_ID 't2'"),
        "42501"
    );
    assert_eq!(
        f.alice_run("TRUNCATE TABLE ca USING OPERATION_ID 't3'"),
        "ok"
    );

    // CASCADE で連鎖して空になる表にも TRUNCATE が要る（親だけの権限では子の行を消せない）。
    f.admin_run("CREATE TABLE par (k BIGINT PRIMARY KEY)");
    f.admin_run("CREATE TABLE ch (parent_id BIGINT REFERENCES par)");
    f.admin_run("GRANT TRUNCATE ON par TO alice");
    assert_eq!(
        f.alice_run("TRUNCATE TABLE par CASCADE USING OPERATION_ID 't4'"),
        "42501"
    );
    f.admin_run("GRANT TRUNCATE ON ch TO alice");
    assert_eq!(
        f.alice_run("TRUNCATE TABLE par CASCADE USING OPERATION_ID 't4'"),
        "ok"
    );
}

#[test]
fn partitioned_dml_is_gated_before_the_job_starts() {
    let f = fx("dmlpriv-part");
    f.admin_run(&ins("ca", 1, "seed"));
    f.admin_run(&ins("ca", 2, "seed2"));
    let sql = "DELETE FROM ca WHERE n > 0 USING OPERATION_ID 'pd1' PARTITIONED CHUNK 1";
    assert_eq!(f.alice_run(sql), "42501");
    assert_eq!(f.count("ca"), 2);
    let explain = "EXPLAIN DELETE FROM ca WHERE n > 0 USING OPERATION_ID 'pd2' PARTITIONED";
    assert_eq!(f.alice_run(explain), "42501");
    f.admin_run("GRANT DELETE ON ca TO alice");
    f.admin_run("GRANT SELECT ON ca TO alice");
    assert_eq!(f.alice_run(sql), "ok");
    assert_eq!(f.count("ca"), 0);
}

#[test]
fn view_dml_needs_privilege_on_the_view_itself() {
    let f = fx("dmlpriv-view");
    f.admin_run("CREATE VIEW va AS SELECT id, embedding, n FROM ca");
    f.admin_run("GRANT INSERT ON ca TO alice");
    let sql = "INSERT INTO va (id, embedding, n) VALUES (1, '[1.0, 0.0]', 1) \
               USING OPERATION_ID 'v1'";
    assert_eq!(f.alice_run(sql), "42501");
    f.admin_run("GRANT INSERT ON va TO alice");
    assert_eq!(f.alice_run(sql), "ok");
}

#[test]
fn explicit_transaction_dml_is_denied_and_nothing_commits() {
    let f = fx("dmlpriv-txn");
    let mut s = bound(f.alice, false);
    let mut txn = f.core.new_session_transaction();
    f.core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let e = f
        .core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, &ins("ca", 1, "tx1"))
        .expect_err("denied");
    assert_eq!(e.wire_code(), "42501");
    // 固定文言で、表名を含まない。
    assert!(!e.client_message().contains("ca"), "{}", e.client_message());
    let _ = f
        .core
        .execute_sql_in_txn(&ctx(), &mut s, &mut txn, "COMMIT");
    assert_eq!(f.count("ca"), 0);
}

#[test]
fn qualified_target_is_checked_under_its_resolved_key() {
    let f = fx("dmlpriv-qual");
    f.admin_run("CREATE SCHEMA app");
    f.admin_run("CREATE TABLE app.q (v BIGINT)");
    let sql = "INSERT INTO app.q (v) VALUES (1) USING OPERATION_ID 'q1'";
    // 修飾名でも名前解決後のキーで ACL を引く（素通しにならない）。
    assert_eq!(f.alice_run(sql), "42501");
    f.admin_run("GRANT INSERT ON app.q TO alice");
    // 表の権限だけでは足りず、SCHEMA の USAGE も要る（Issue #2234）。
    assert_eq!(f.alice_run(sql), "42501");
    f.admin_run("GRANT USAGE ON SCHEMA app TO alice");
    let r = f.alice_run(sql);
    assert_eq!(r, "ok");
}

#[test]
fn missing_table_still_reports_42p01_before_privilege() {
    let f = fx("dmlpriv-missing");
    assert_eq!(f.alice_run(&ins("nope", 1, "m1")), "42P01");
}

#[test]
fn quoted_only_table_with_alias_is_checked_as_a_relation() {
    // 実表 `"only"` を別名 `ca` で読む文は、別名と同名の表 `ca` の権限では通らない（`ONLY` を構文語と
    // 誤認して検査対象から落とさない。Issue #1968 のレビュー指摘）。
    let f = fx("dmlpriv-only");
    f.admin_run("CREATE TABLE \"only\" (k BIGINT PRIMARY KEY)");
    f.admin_run("GRANT SELECT ON ca TO alice");
    assert_eq!(f.alice_run("SELECT k FROM \"only\" ca LIMIT 5"), "42501");
    f.admin_run("GRANT SELECT ON \"only\" TO alice");
    assert_ne!(f.alice_run("SELECT k FROM \"only\" ca LIMIT 5"), "42501");
}
