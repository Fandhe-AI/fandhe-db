//! 明示トランザクション内の関数・ポリシー・権限・ロール・トリガの DDL（SQL-80・SQL-31・SQL-23・SQL-52・
//! TABLE-22・TABLE-36・TABLE-44・TASK-349 のポインタ、Issue #3316。設計記録:
//! `docs/design/explicit-transaction.md`「#3316 の実装記録」）の結合テスト。
//!
//! `EngineCore::execute_sql_in_txn`（明示 Tx の production 入口）で、受理した種別が COMMIT で効き、ROLLBACK・
//! 失敗では痕跡を残さないこと、権限・ポリシー・ロールのメンバーシップの変更が COMMIT 前には別セッションの判定を
//! 変えないこと（越境試験）、積んだ後の後続文を古い判定で通さず固定文言の `0A000` にすること（fail-closed）を固定する。
//! 受理しない形（PL/pgSQL・`DO` の中の DDL）の `0A000` には注記を付ける。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ALICE: u64 = 1;
const BOB: u64 = 2;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 運用者権限（`--ddl-allowed-users`）を持つ未束縛セッション。
fn operator() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

/// 現在ロールを束縛したセッション（`ddl` なら運用者権限つき）。
fn bound(id: u64, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    if ddl {
        s.allow_ddl();
    }
    s
}

struct Fx {
    _g: CleanupGuard,
    core: EngineCore,
}

/// t1 に alice・bob（ID は 1・2）、t2 に carol を作った DB。
fn fx(label: &str) -> Fx {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let f = Fx { _g: guard, core };
    let mut op = operator();
    f.ok("t1", &mut op, "CREATE ROLE alice");
    f.ok("t1", &mut op, "CREATE ROLE bob");
    f.ok("t2", &mut op, "CREATE ROLE carol");
    f
}

impl Fx {
    fn run(
        &self,
        tenant: &str,
        s: &mut SessionState,
        sql: &str,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        self.core.execute_sql_in_session(&ctx(tenant), s, sql)
    }

    fn ok(&self, tenant: &str, s: &mut SessionState, sql: &str) {
        if let Err(e) = self.run(tenant, s, sql) {
            panic!("{sql} must succeed: {} {e}", e.wire_code());
        }
    }

    /// 成功なら `"ok"`、失敗なら SQLSTATE。
    fn status(&self, tenant: &str, s: &mut SessionState, sql: &str) -> &'static str {
        match self.run(tenant, s, sql) {
            Ok(_) => "ok",
            Err(e) => e.wire_code(),
        }
    }

    /// `SELECT` の結果行数（失敗なら SQLSTATE）。
    fn rows(&self, tenant: &str, s: &mut SessionState, sql: &str) -> Result<usize, &'static str> {
        match self.run(tenant, s, sql) {
            Ok(SqlOutcome::Query(q)) => Ok(q.rows.len()),
            Ok(other) => panic!("{sql}: unexpected {other:?}"),
            Err(e) => Err(e.wire_code()),
        }
    }

    fn begin<'a>(&'a self, tenant: &str, s: &mut SessionState) -> SessionTransaction<'a> {
        let mut txn = self.core.new_session_transaction();
        self.core
            .execute_sql_in_txn(&ctx(tenant), s, &mut txn, "BEGIN")
            .expect("begin");
        txn
    }

    fn tx<'a>(
        &'a self,
        tenant: &str,
        s: &mut SessionState,
        txn: &mut SessionTransaction<'a>,
        sql: &str,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        self.core.execute_sql_in_txn(&ctx(tenant), s, txn, sql)
    }

    fn tx_ok<'a>(
        &'a self,
        tenant: &str,
        s: &mut SessionState,
        txn: &mut SessionTransaction<'a>,
        sql: &str,
    ) {
        if let Err(e) = self.tx(tenant, s, txn, sql) {
            panic!("{sql} must succeed in a transaction: {} {e}", e.wire_code());
        }
    }

    fn tx_code<'a>(
        &'a self,
        tenant: &str,
        s: &mut SessionState,
        txn: &mut SessionTransaction<'a>,
        sql: &str,
    ) -> SqlSurfaceError {
        match self.tx(tenant, s, txn, sql) {
            Ok(o) => panic!("{sql} must fail in a transaction, got {o:?}"),
            Err(e) => e,
        }
    }
}

/// GRANT は COMMIT 前には別セッション（同じテナント・別テナント）の判定を変えず、ROLLBACK の後も変えない。
#[test]
fn grant_is_invisible_to_other_sessions_until_commit() {
    let f = fx("sql80_sec_grant");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    let mut carol = SessionState::default();
    carol.bind_role_ids(3, 3);
    f.ok("t1", &mut alice, "CREATE TABLE docs (a TEXT)");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Err("42501"));
    let carol_before = f.status("t2", &mut carol, "SELECT a FROM docs");

    // ROLLBACK: 積んだ GRANT は誰にも効かず、何も残らない。
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok("t1", &mut alice, &mut txn, "GRANT SELECT ON docs TO bob");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Err("42501"));
    assert_eq!(
        f.status("t2", &mut carol, "SELECT a FROM docs"),
        carol_before
    );
    f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Err("42501"));

    // COMMIT: 原子的に確定し、別セッションの判定が変わる。別テナントの応答は変わらない。
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok("t1", &mut alice, &mut txn, "GRANT SELECT ON docs TO bob");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Err("42501"));
    f.tx_ok("t1", &mut alice, &mut txn, "COMMIT");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Ok(0));
    assert_eq!(
        f.status("t2", &mut carol, "SELECT a FROM docs"),
        carol_before
    );

    // REVOKE も同じ（COMMIT 前には取り消されず、COMMIT で効く）。
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok("t1", &mut alice, &mut txn, "REVOKE SELECT ON docs FROM bob");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Ok(0));
    f.tx_ok("t1", &mut alice, &mut txn, "COMMIT");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Err("42501"));
}

/// ポリシーの緩和・行セキュリティの無効化は COMMIT 前には別セッションの可視行を変えない。
#[test]
fn policy_relaxation_does_not_widen_other_sessions_before_commit() {
    let f = fx("sql80_sec_policy");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    f.ok("t1", &mut alice, "CREATE TABLE docs (a TEXT)");
    f.ok("t1", &mut alice, "INSERT INTO docs (a) VALUES ('x')");
    f.ok("t1", &mut alice, "INSERT INTO docs (a) VALUES ('y')");
    f.ok("t1", &mut alice, "GRANT SELECT ON docs TO bob");
    f.ok(
        "t1",
        &mut alice,
        "CREATE POLICY only_x ON docs FOR SELECT USING (a = 'x')",
    );
    f.ok(
        "t1",
        &mut alice,
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
    );
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Ok(1));

    for relax in [
        "ALTER POLICY only_x ON docs USING (a IS NOT NULL)",
        "ALTER TABLE docs DISABLE ROW LEVEL SECURITY",
    ] {
        // ROLLBACK: 緩和は誰にも効かない。
        let mut txn = f.begin("t1", &mut alice);
        f.tx_ok("t1", &mut alice, &mut txn, relax);
        assert_eq!(
            f.rows("t1", &mut bob, "SELECT a FROM docs"),
            Ok(1),
            "{relax}"
        );
        f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK");
        assert_eq!(
            f.rows("t1", &mut bob, "SELECT a FROM docs"),
            Ok(1),
            "{relax}"
        );
    }
    // COMMIT: 緩和が効いて全行が見える。
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok(
        "t1",
        &mut alice,
        &mut txn,
        "ALTER TABLE docs DISABLE ROW LEVEL SECURITY",
    );
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Ok(1));
    f.tx_ok("t1", &mut alice, &mut txn, "COMMIT");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Ok(2));
}

/// ロールのメンバーシップ付与は COMMIT 前には別セッションの `SET ROLE` の判定を変えない。
#[test]
fn role_membership_grant_does_not_affect_other_sessions_before_commit() {
    let f = fx("sql80_sec_member");
    let mut admin = bound(BOB, true);
    let mut alice = bound(ALICE, false);
    assert_eq!(f.status("t1", &mut alice, "SET ROLE bob"), "42501");
    let mut txn = f.begin("t1", &mut admin);
    f.tx_ok("t1", &mut admin, &mut txn, "GRANT bob TO alice");
    assert_eq!(f.status("t1", &mut alice, "SET ROLE bob"), "42501");
    f.tx_ok("t1", &mut admin, &mut txn, "ROLLBACK");
    assert_eq!(f.status("t1", &mut alice, "SET ROLE bob"), "42501");
    let mut txn = f.begin("t1", &mut admin);
    f.tx_ok("t1", &mut admin, &mut txn, "GRANT bob TO alice");
    f.tx_ok("t1", &mut admin, &mut txn, "COMMIT");
    assert_eq!(f.status("t1", &mut alice, "SET ROLE bob"), "ok");
}

/// ロール DDL は COMMIT まで別セッションに見えない。別テナントにも見えない（表の足跡が無くても COMMIT できる）。
/// 別セッションの観測は読み取りだけの `SET ROLE` で行う（autocommit の書き込みは writer を待つため）。
#[test]
fn role_ddl_is_invisible_until_commit_and_commits_without_a_table_footprint() {
    let f = fx("sql80_sec_role");
    let mut op = operator();
    let mut alice = bound(ALICE, false);
    let mut carol = bound(3, false);
    let mut txn = f.begin("t1", &mut op);
    f.tx_ok("t1", &mut op, &mut txn, "CREATE ROLE dave");
    // 別セッションからは未作成と同じ（`42704`）。別テナントも同じ。
    assert_eq!(f.status("t1", &mut alice, "SET ROLE dave"), "42704");
    assert_eq!(f.status("t2", &mut carol, "SET ROLE dave"), "42704");
    f.tx_ok("t1", &mut op, &mut txn, "COMMIT");
    // COMMIT 後は t1 で存在する（メンバーではないので `42501`）。別テナントは変わらない。
    assert_eq!(f.status("t1", &mut alice, "SET ROLE dave"), "42501");
    assert_eq!(f.status("t2", &mut carol, "SET ROLE dave"), "42704");
}

/// 権限・ポリシー・ロール等を積んだ後の後続文は、古い判定で通さず固定文言の `0A000` にする（名前・存在を含めない）。
/// `ROLLBACK TO` で制限が解ける。
// tracked: #3533
#[test]
fn followers_after_guarded_ddl_are_rejected_with_a_fixed_message() {
    let f = fx("sql80_sec_guard");
    let mut alice = bound(ALICE, true);
    f.ok("t1", &mut alice, "CREATE TABLE docs (a TEXT)");
    f.ok("t1", &mut alice, "GRANT SELECT ON docs TO bob");
    let mut messages = Vec::new();
    for follower in [
        "SELECT a FROM docs",
        "SELECT count(*) FROM docs",
        "INSERT INTO docs (a) VALUES ('x')",
        "SET ROLE bob",
    ] {
        let mut txn = f.begin("t1", &mut alice);
        f.tx_ok("t1", &mut alice, &mut txn, "SAVEPOINT s");
        f.tx_ok("t1", &mut alice, &mut txn, "REVOKE SELECT ON docs FROM bob");
        let e = f.tx_code("t1", &mut alice, &mut txn, follower);
        assert_eq!(e.wire_code(), "0A000", "{follower}");
        messages.push(e.to_string());
        // `ROLLBACK TO` で GRANT/REVOKE の印と書き込みが戻り、後続文が通る。
        f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK TO s");
        f.tx_ok("t1", &mut alice, &mut txn, "SELECT a FROM docs");
        f.tx_ok("t1", &mut alice, &mut txn, "COMMIT");
        // REVOKE は取り消されているので bob の権限は残っている。
        let mut bob = bound(BOB, false);
        assert_eq!(
            f.rows("t1", &mut bob, "SELECT a FROM docs"),
            Ok(0),
            "{follower}"
        );
    }
    // 文言は文の種類だけで、対象の名前・存在を含まない（存在しない表でも同じ）。
    assert_eq!(messages[0], messages[1]);
    assert!(messages
        .iter()
        .all(|m| !m.contains("docs") && !m.contains("nosuch")));
    // DDL・セッション変数の SET/SHOW は後続でも通る。
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok("t1", &mut alice, &mut txn, "REVOKE SELECT ON docs FROM bob");
    f.tx_ok("t1", &mut alice, &mut txn, "SET TIME ZONE 'UTC'");
    f.tx_ok("t1", &mut alice, &mut txn, "CREATE TABLE more (a TEXT)");
    f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK");
}

/// 何も書き込まない no-op のセキュリティ DDL（オプションなしの `ALTER ROLE`・全名不在の
/// `DROP ROLE IF EXISTS`・反映済みの GRANT／REVOKE／`ALTER DEFAULT PRIVILEGES` の再実行）は guard 印を残さず、
/// 同じ Tx の後続の SELECT・INSERT が `0A000` にならない（Issue #3316 の Bugbot 指摘）。
#[test]
fn noop_security_ddl_does_not_block_followers() {
    let f = fx("sql80_sec_noop");
    let mut alice = bound(ALICE, true);
    f.ok("t1", &mut alice, "CREATE TABLE docs (a TEXT)");
    f.ok("t1", &mut alice, "GRANT SELECT ON docs TO bob");
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO bob",
    );
    for noop in [
        "ALTER ROLE bob",
        "DROP ROLE IF EXISTS nosuch1, nosuch2",
        "GRANT SELECT ON docs TO bob",
        "REVOKE INSERT ON docs FROM bob",
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO bob",
        "ALTER DEFAULT PRIVILEGES REVOKE INSERT ON TABLES FROM bob",
    ] {
        let mut txn = f.begin("t1", &mut alice);
        f.tx_ok("t1", &mut alice, &mut txn, noop);
        f.tx_ok("t1", &mut alice, &mut txn, "SELECT a FROM docs");
        f.tx_ok(
            "t1",
            &mut alice,
            &mut txn,
            "INSERT INTO docs (a) VALUES ('x')",
        );
        f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK");
    }
}

/// 1 つの Tx で表・ポリシー・行セキュリティ・GRANT を積み、原子的に効く。既定権限は同じ Tx で作る表に付く。
#[test]
fn combined_ddl_commits_atomically() {
    let f = fx("sql80_sec_combo");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    let mut txn = f.begin("t1", &mut alice);
    for sql in [
        "CREATE TABLE t (a TEXT)",
        "CREATE POLICY p ON t FOR SELECT USING (a = 'x')",
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "GRANT SELECT ON t TO bob",
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO bob",
        "CREATE TABLE u (a TEXT)",
    ] {
        f.tx_ok("t1", &mut alice, &mut txn, sql);
    }
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM t"), Err("42P01"));
    f.tx_ok("t1", &mut alice, &mut txn, "COMMIT");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM t"), Ok(0));
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM u"), Ok(0));
}

/// 名前解決は確定済みのカタログで行われる。同じ Tx で作った public 表と同名の SCHEMA 配下の表が `search_path` で
/// 見えるとき、その名前の本種別の DDL が SCHEMA 配下の表へ当たってはならない（別の表の権限・ポリシーを変える
/// fail-open）。表・索引の DDL を積んだ後は、SCHEMA 配下の内部キーに解決された対象を固定の拒否にする。
// tracked: #3533
#[test]
fn names_resolved_to_schema_tables_after_table_ddl_are_not_applied_to_the_wrong_table() {
    let f = fx("sql80_sec_shadow");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    f.ok("t1", &mut alice, "CREATE SCHEMA s");
    f.ok("t1", &mut alice, "CREATE TABLE s.t (a TEXT)");
    f.ok("t1", &mut alice, "GRANT USAGE ON SCHEMA s TO bob");
    f.ok("t1", &mut alice, "SET search_path = public, s");
    for sql in [
        "GRANT SELECT ON t TO bob",
        "CREATE POLICY p ON t FOR SELECT USING (a = 'x')",
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE t OWNER TO bob",
    ] {
        let mut txn = f.begin("t1", &mut alice);
        f.tx_ok("t1", &mut alice, &mut txn, "CREATE TABLE t (a TEXT)");
        let status = f.tx("t1", &mut alice, &mut txn, sql);
        // 拒否は固定の `0A000`（SCHEMA 配下の内部キーに解決された対象）。受理するなら public の `t` へ当たること。
        let code = status.map(|_| "ok").unwrap_or_else(|e| e.wire_code());
        // 名前を `search_path` で解決する文（GRANT・OWNER TO）は SCHEMA 配下の `s.t` へ当たりうるため必ず拒否する。
        // ポリシー・行セキュリティは未修飾の名前を public の表として扱うため、受理されても public の `t` へ当たる。
        if sql.starts_with("GRANT") || sql.contains("OWNER TO") {
            assert_eq!(code, "0A000", "{sql}");
        } else {
            assert!(code == "ok" || code == "0A000", "{sql}: {code}");
        }
        // 失敗した Tx は ROLLBACK だけが通る。
        f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK");
        // `s.t` には何も効いていない。
        assert_eq!(
            f.rows("t1", &mut bob, "SELECT a FROM s.t"),
            Err("42501"),
            "{sql}"
        );
    }
}

/// SAVEPOINT／ROLLBACK TO: 巻き戻した範囲の GRANT は残らず、後続文の制限も解ける。
#[test]
fn rollback_to_savepoint_undoes_privilege_changes() {
    let f = fx("sql80_sec_savepoint");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    f.ok("t1", &mut alice, "CREATE TABLE docs (a TEXT)");
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok("t1", &mut alice, &mut txn, "SAVEPOINT s");
    f.tx_ok("t1", &mut alice, &mut txn, "GRANT SELECT ON docs TO bob");
    f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK TO s");
    f.tx_ok("t1", &mut alice, &mut txn, "SELECT a FROM docs");
    f.tx_ok("t1", &mut alice, &mut txn, "COMMIT");
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Err("42501"));
}

/// 判定順: READ ONLY Tx は `25006`、権限の無い主体は対象の有無を問わず `42501`（writer を取らない）。
#[test]
fn gates_precede_the_writer_and_the_catalog() {
    let f = fx("sql80_sec_gates");
    let mut alice = bound(ALICE, true);
    f.ok("t1", &mut alice, "CREATE TABLE docs (a TEXT)");
    for sql in [
        "GRANT SELECT ON docs TO bob",
        "CREATE POLICY p ON docs",
        "CREATE ROLE dave",
        "DROP FUNCTION nosuch",
    ] {
        let mut ro = f.core.new_session_transaction();
        f.core
            .execute_sql_in_txn(&ctx("t1"), &mut alice, &mut ro, "BEGIN READ ONLY")
            .expect("begin");
        let e = f.tx_code("t1", &mut alice, &mut ro, sql);
        assert_eq!(e.wire_code(), "25006", "{sql}");
        f.tx_ok("t1", &mut alice, &mut ro, "ROLLBACK");
    }
    // 権限なし（未束縛・DDL 権限なし）は対象の有無を問わず `42501`。
    let mut denied = SessionState::default();
    for sql in [
        "GRANT SELECT ON docs TO bob",
        "GRANT SELECT ON nosuch TO bob",
        "CREATE POLICY p ON nosuch",
        "CREATE ROLE dave",
        "ALTER ROLE nosuch LOGIN",
        "CREATE TRIGGER g BEFORE INSERT ON nosuch EXECUTE FUNCTION f()",
        "DROP TRIGGER g ON nosuch",
        "ALTER TABLE nosuch ENABLE ROW LEVEL SECURITY",
    ] {
        let mut txn = f.begin("t1", &mut denied);
        let e = f.tx_code("t1", &mut denied, &mut txn, sql);
        assert_eq!(e.wire_code(), "42501", "{sql}");
        f.tx_ok("t1", &mut denied, &mut txn, "ROLLBACK");
    }
}

/// `DO` ブロックの中の本種別の DDL は受け付けない（PL/pgSQL・動的 `EXECUTE` も同じ分類で拒否される）。
/// intentional: docs/design/explicit-transaction.md
#[test]
fn ddl_inside_do_blocks_stays_unsupported() {
    let f = fx("sql80_sec_do");
    let mut alice = bound(ALICE, true);
    f.ok("t1", &mut alice, "CREATE TABLE docs (a TEXT)");
    let mut txn = f.begin("t1", &mut alice);
    let e = f.tx_code(
        "t1",
        &mut alice,
        &mut txn,
        "DO $$ BEGIN GRANT SELECT ON docs TO bob; END $$",
    );
    assert_eq!(e.wire_code(), "0A000");
    f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK");
    let mut bob = bound(BOB, false);
    assert_eq!(f.rows("t1", &mut bob, "SELECT a FROM docs"), Err("42501"));
}

/// 関数・トリガ: 同じ Tx で作ったトリガ関数にトリガを張れ、ROLLBACK では何も残らず、COMMIT で確定する。
/// 積んだ後の INSERT は古いトリガで発火させず `0A000`。表の DDL の後の `CREATE FUNCTION` は `0A000`。
/// tracked: #3533
#[test]
fn trigger_and_function_ddl_commit_and_rollback() {
    let f = fx("sql80_sec_trigger");
    let mut alice = bound(ALICE, true);
    f.ok("t1", &mut alice, "CREATE SCHEMA sa");
    f.ok("t1", &mut alice, "CREATE TABLE sa.t (a TEXT)");
    let func =
        "CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$";
    let create = "CREATE TRIGGER g BEFORE INSERT ON sa.t FOR EACH ROW EXECUTE FUNCTION trg()";
    // ROLLBACK: 関数もトリガも残らない。
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok("t1", &mut alice, &mut txn, func);
    f.tx_ok("t1", &mut alice, &mut txn, create);
    let e = f.tx_code(
        "t1",
        &mut alice,
        &mut txn,
        "INSERT INTO sa.t (a) VALUES ('x')",
    );
    assert_eq!(e.wire_code(), "0A000");
    f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK");
    assert_eq!(
        f.status("t1", &mut alice, "DROP TRIGGER g ON sa.t"),
        "42704"
    );
    assert_eq!(f.status("t1", &mut alice, "DROP FUNCTION trg"), "42883");
    // COMMIT: 確定する。
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok("t1", &mut alice, &mut txn, func);
    f.tx_ok("t1", &mut alice, &mut txn, create);
    f.tx_ok("t1", &mut alice, &mut txn, "COMMIT");
    f.ok("t1", &mut alice, "INSERT INTO sa.t (a) VALUES ('x')");
    // DROP TRIGGER／DROP FUNCTION も Tx 内で積め、ROLLBACK では残る。依存トリガがあると RESTRICT は `2BP01`。
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok("t1", &mut alice, &mut txn, "DROP TRIGGER g ON sa.t");
    f.tx_ok("t1", &mut alice, &mut txn, "DROP FUNCTION trg");
    f.tx_ok("t1", &mut alice, &mut txn, "ROLLBACK");
    assert_eq!(f.status("t1", &mut alice, "DROP FUNCTION trg"), "2BP01");
    // 表の DDL の後の `CREATE FUNCTION` は関数本体の束縛が確定済みカタログを読むため `0A000`。
    let mut txn = f.begin("t1", &mut alice);
    f.tx_ok("t1", &mut alice, &mut txn, "CREATE TABLE newt (a TEXT)");
    let e = f.tx_code("t1", &mut alice, &mut txn, "CREATE FUNCTION g2(x) AS x");
    assert_eq!(e.wire_code(), "0A000");
}

/// RLS-13（Issue #2275）: 同名の一時表が永続表を隠しているとき、Tx 内の行セキュリティ変更も自動コミットと
/// 同じ `0A000` で拒否し、永続表の有効化状態を黙って変えない（fail-open の防止）。
#[test]
fn row_security_in_txn_rejects_when_temp_table_shadows() {
    let f = fx("sql80-txn-rls-temp");
    let mut op = operator();
    f.ok("t1", &mut op, "CREATE TABLE tt (a TEXT)");
    f.ok("t1", &mut op, "CREATE TEMP TABLE tt (a TEXT)");
    for sql in [
        "ALTER TABLE tt ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE tt DISABLE ROW LEVEL SECURITY",
        "ALTER TABLE tt FORCE ROW LEVEL SECURITY",
        "ALTER TABLE tt NO FORCE ROW LEVEL SECURITY",
    ] {
        let mut txn = f.begin("t1", &mut op);
        let e = f.tx_code("t1", &mut op, &mut txn, sql);
        assert_eq!(e.wire_code(), "0A000", "{sql}");
        f.tx_ok("t1", &mut op, &mut txn, "ROLLBACK");
    }
}
