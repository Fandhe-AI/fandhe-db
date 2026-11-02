//! `ALTER DEFAULT PRIVILEGES`（SQL-40・RLS-12 のポインタ、Issue #2237）の SQL 水準の結合テスト。設計記録:
//! `docs/design/role-privilege-model.md` §19。
//!
//! 固定する契約: 既定権限は設定「後」に作ったオブジェクト（表・CTAS・ビュー・SERIAL の暗黙 SEQUENCE・
//! SEQUENCE・関数・SCHEMA）の初期 ACL にだけ効き既存には効かない・全体と `IN SCHEMA` の合成・
//! `CREATE OR REPLACE` では適用しない・`FOR ROLE` は SET 経路（運用者権限は免除）・存在オラクルにならない
//! （他テナントのロールは不在と同じ `42704`、他テナントの SCHEMA は `3F000`、未束縛はカタログ照会より前に
//! `42501`）・1 文の失敗で何も保存しない・明示 Tx 内は `0A000`・`DROP ROLE` は既定権限の対象を `2BP01` で拒否し
//! `DROP SCHEMA` は当該 SCHEMA の既定権限を消す。ロールは `CREATE ROLE` で作り、ID は作成順の 1〜。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ALICE: u64 = 1;
const BOB: u64 = 2;
const CAROL_T2: u64 = 3;

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

/// 関数カタログを読み込んだ bob のセッション（UDF はセッション単位のレジストリへ読み込む）。
fn bob_loaded(f: &Fx) -> SessionState {
    let mut s = bound(BOB, false);
    f.core
        .load_session_udfs(&ctx("t1"), &mut s)
        .expect("load udfs");
    s
}

struct Fx {
    _g: CleanupGuard,
    core: EngineCore,
}

/// t1 に alice・bob、t2 に carol を作った DB（ID は作成順に 1・2・3）。
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

    fn code(&self, tenant: &str, s: &mut SessionState, sql: &str) -> &'static str {
        match self.run(tenant, s, sql) {
            Ok(o) => panic!("{sql} must fail, got {o:?}"),
            Err(e) => e.wire_code(),
        }
    }
}

/// 受入基準: 既定権限の設定後に作った表には付与が反映され、既存の表には影響しない。
#[test]
fn default_grant_applies_to_new_tables_only() {
    let f = fx("sql40_dp_accept");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    f.ok("t1", &mut alice, "CREATE TABLE t_old (a TEXT)");
    let out = f.run(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO bob",
    );
    assert!(matches!(out, Ok(SqlOutcome::AlterDefaultPrivileges(_))));
    f.ok("t1", &mut alice, "CREATE TABLE t_new (a TEXT)");
    f.ok("t1", &mut bob, "SELECT a FROM t_new");
    assert_eq!(f.code("t1", &mut bob, "SELECT a FROM t_old"), "42501");
    // 付与されたのは SELECT だけ。
    assert_eq!(
        f.code("t1", &mut bob, "INSERT INTO t_new (a) VALUES ('x')"),
        "42501"
    );
    // 取り消した後に作った表には付かない（作成済みの表は既に付いたまま）。
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES REVOKE SELECT ON TABLES FROM bob",
    );
    f.ok("t1", &mut alice, "CREATE TABLE t_later (a TEXT)");
    assert_eq!(f.code("t1", &mut bob, "SELECT a FROM t_later"), "42501");
    f.ok("t1", &mut bob, "SELECT a FROM t_new");
}

/// 既定権限は作成者（対象ロール）ごと。別ロールの作成には作用しない。CTAS にも適用される。
#[test]
fn default_privileges_are_per_creating_role_and_cover_ctas() {
    let f = fx("sql40_dp_ctas");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, true);
    f.ok("t1", &mut alice, "CREATE TABLE base (a TEXT)");
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC",
    );
    f.ok("t1", &mut alice, "CREATE TABLE c1 AS SELECT a FROM base");
    f.ok("t1", &mut bob, "CREATE TABLE b1 (a TEXT)");
    let mut carol_like = bound(BOB, false);
    f.ok("t1", &mut carol_like, "SELECT a FROM c1");
    // bob の作成（対象ロールが違う）には alice の既定は作用しない。alice は所有者ではないので読めない。
    let mut alice_plain = bound(ALICE, false);
    assert_eq!(f.code("t1", &mut alice_plain, "SELECT a FROM b1"), "42501");
}

/// 全体エントリとスキーマ別エントリの合成。スキーマ別の REVOKE はスキーマ別の付与だけを消す。
#[test]
fn schema_scoped_and_global_entries_compose() {
    let f = fx("sql40_dp_scoped");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    f.ok("t1", &mut alice, "CREATE SCHEMA s");
    f.ok("t1", &mut alice, "GRANT USAGE ON SCHEMA s TO bob");
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES IN SCHEMA s GRANT SELECT ON TABLES TO bob",
    );
    f.ok("t1", &mut alice, "CREATE TABLE s.t1 (a TEXT)");
    f.ok("t1", &mut alice, "CREATE TABLE p1 (a TEXT)");
    f.ok("t1", &mut bob, "SELECT a FROM s.t1");
    assert_eq!(f.code("t1", &mut bob, "SELECT a FROM p1"), "42501");
    // 全体エントリ（INSERT）を足すと、s 配下の新しい表は両方を持ち、public の新しい表は INSERT だけを持つ。
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT INSERT ON TABLES TO bob",
    );
    f.ok("t1", &mut alice, "CREATE TABLE s.t2 (a TEXT)");
    f.ok("t1", &mut alice, "CREATE TABLE p2 (a TEXT)");
    f.ok("t1", &mut bob, "SELECT a FROM s.t2");
    f.ok("t1", &mut bob, "INSERT INTO s.t2 (a) VALUES ('x')");
    f.ok("t1", &mut bob, "INSERT INTO p2 (a) VALUES ('x')");
    assert_eq!(f.code("t1", &mut bob, "SELECT a FROM p2"), "42501");
    // スキーマ別の SELECT だけを取り消す。全体の INSERT は残る。
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES IN SCHEMA s REVOKE SELECT ON TABLES FROM bob",
    );
    f.ok("t1", &mut alice, "CREATE TABLE s.t3 (a TEXT)");
    assert_eq!(f.code("t1", &mut bob, "SELECT a FROM s.t3"), "42501");
    f.ok("t1", &mut bob, "INSERT INTO s.t3 (a) VALUES ('x')");
}

/// ビューにも `TABLES` の既定が付く。`CREATE OR REPLACE VIEW` の置換には適用しない。
#[test]
fn views_get_table_defaults_but_replacement_does_not() {
    let f = fx("sql40_dp_view");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    f.ok("t1", &mut alice, "CREATE TABLE base (a TEXT)");
    f.ok("t1", &mut alice, "GRANT SELECT ON base TO bob");
    f.ok("t1", &mut alice, "CREATE VIEW v_old AS SELECT a FROM base");
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO bob",
    );
    f.ok("t1", &mut alice, "CREATE VIEW v_new AS SELECT a FROM base");
    f.ok("t1", &mut bob, "SELECT a FROM v_new");
    assert_eq!(f.code("t1", &mut bob, "SELECT a FROM v_old"), "42501");
    // 置換は既存のビューの ACL を変えない（新規の作成ではない）。
    f.ok(
        "t1",
        &mut alice,
        "CREATE OR REPLACE VIEW v_old AS SELECT a FROM base",
    );
    assert_eq!(f.code("t1", &mut bob, "SELECT a FROM v_old"), "42501");
}

/// `REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC` の後に作った関数は呼べない。既存の関数は従来どおり呼べる。
#[test]
fn function_default_can_be_revoked_from_public() {
    let f = fx("sql40_dp_function");
    let mut alice = bound(ALICE, true);
    f.ok("t1", &mut alice, "CREATE TABLE ft (n BIGINT)");
    f.ok("t1", &mut alice, "INSERT INTO ft (n) VALUES (1)");
    f.ok("t1", &mut alice, "GRANT SELECT ON ft TO PUBLIC");
    f.ok("t1", &mut alice, "CREATE FUNCTION f_old(x) AS x + 1");
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC",
    );
    f.ok("t1", &mut alice, "CREATE FUNCTION f_new(x) AS x + 1");
    f.ok("t1", &mut bob_loaded(&f), "SELECT f_old(n) FROM ft");
    assert_eq!(
        f.code("t1", &mut bob_loaded(&f), "SELECT f_new(n) FROM ft"),
        "42501"
    );
    // 所有者（作成者）は呼べる。
    f.ok("t1", &mut alice, "SELECT f_new(n) FROM ft");
    // bob にだけ既定で付与すると、その後の関数は bob だけが呼べる（PUBLIC の既定は取り消し済み）。
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON FUNCTIONS TO bob",
    );
    f.ok("t1", &mut alice, "CREATE FUNCTION f_bob(x) AS x + 1");
    f.ok("t1", &mut bob_loaded(&f), "SELECT f_bob(n) FROM ft");
    // PUBLIC へ戻すと組み込みの既定（誰でも実行可）に戻る。
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON FUNCTIONS TO PUBLIC",
    );
    f.ok("t1", &mut alice, "CREATE FUNCTION f_pub(x) AS x + 1");
    f.ok("t1", &mut bob_loaded(&f), "SELECT f_pub(n) FROM ft");
}

/// SERIAL の暗黙 SEQUENCE と `CREATE SEQUENCE` に `SEQUENCES` の既定が付く（USAGE で `nextval`）。
#[test]
fn sequence_defaults_apply_to_serial_and_create_sequence() {
    let f = fx("sql40_dp_sequence");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    f.ok("t1", &mut alice, "CREATE SEQUENCE q_old");
    f.ok("t1", &mut alice, "CREATE TABLE ts_old (id SERIAL, a TEXT)");
    // 表単位の権限は SEQUENCE の権限と別軸なので、SEQUENCE の検査だけを観測できるよう PUBLIC へ付与する。
    f.ok("t1", &mut alice, "CREATE TABLE sink (n BIGINT)");
    f.ok("t1", &mut alice, "GRANT INSERT, SELECT ON sink TO PUBLIC");
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT USAGE ON SEQUENCES TO bob",
    );
    f.ok("t1", &mut alice, "CREATE SEQUENCE q_new");
    f.ok("t1", &mut alice, "CREATE TABLE ts_new (id SERIAL, a TEXT)");
    let ins = |q: &str| format!("INSERT INTO sink (n) VALUES (nextval('{q}'))");
    f.ok("t1", &mut bob, &ins("q_new"));
    f.ok("t1", &mut bob, &ins("ts_new_id_seq"));
    assert_eq!(f.code("t1", &mut bob, &ins("q_old")), "42501");
    assert_eq!(f.code("t1", &mut bob, &ins("ts_old_id_seq")), "42501");
}

/// `ON SCHEMAS` の既定は新しい SCHEMA に付く（USAGE。既存の SCHEMA には付かない）。
#[test]
fn schema_defaults_apply_to_new_schemas() {
    let f = fx("sql40_dp_schemas");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    f.ok("t1", &mut alice, "CREATE SCHEMA s_old");
    f.ok("t1", &mut alice, "CREATE TABLE s_old.t (a TEXT)");
    f.ok("t1", &mut alice, "GRANT SELECT ON s_old.t TO bob");
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT USAGE ON SCHEMAS TO bob",
    );
    f.ok("t1", &mut alice, "CREATE SCHEMA s_new");
    f.ok("t1", &mut alice, "CREATE TABLE s_new.t (a TEXT)");
    f.ok("t1", &mut alice, "GRANT SELECT ON s_new.t TO bob");
    f.ok("t1", &mut bob, "SELECT a FROM s_new.t");
    assert_eq!(f.code("t1", &mut bob, "SELECT a FROM s_old.t"), "42501");
}

/// `FOR ROLE`: SET 経路の無い対象は `42501`（副作用なし）、運用者権限は免除、メンバー（SET あり）は成功する。
#[test]
fn for_role_requires_a_set_path_unless_operator() {
    let f = fx("sql40_dp_for_role");
    let mut alice = bound(ALICE, false);
    let mut alice_op = bound(ALICE, true);
    let mut bob_op = bound(BOB, true);
    assert_eq!(
        f.code(
            "t1",
            &mut alice,
            "ALTER DEFAULT PRIVILEGES FOR ROLE bob GRANT SELECT ON TABLES TO alice"
        ),
        "42501"
    );
    // 副作用なし: bob が作る表を alice は読めない。
    f.ok("t1", &mut bob_op, "CREATE TABLE b0 (a TEXT)");
    assert_eq!(f.code("t1", &mut alice, "SELECT a FROM b0"), "42501");
    // 運用者権限は SET 経路の検査を免除する（束縛は要る）。
    f.ok(
        "t1",
        &mut alice_op,
        "ALTER DEFAULT PRIVILEGES FOR ROLE bob GRANT SELECT ON TABLES TO alice",
    );
    f.ok("t1", &mut bob_op, "CREATE TABLE b1 (a TEXT)");
    f.ok("t1", &mut alice, "SELECT a FROM b1");
    assert_eq!(f.code("t1", &mut alice, "SELECT a FROM b0"), "42501");
    // メンバー（SET 可能）は運用者権限なしで設定できる。
    f.ok("t1", &mut alice_op, "GRANT alice TO bob");
    let mut bob = bound(BOB, false);
    f.ok(
        "t1",
        &mut bob,
        "ALTER DEFAULT PRIVILEGES FOR ROLE alice GRANT INSERT ON TABLES TO bob",
    );
    f.ok("t1", &mut alice_op, "CREATE TABLE a1 (a TEXT)");
    f.ok("t1", &mut bob, "INSERT INTO a1 (a) VALUES ('x')");
}

/// テナント境界（RLS-12）: 他テナントのロール・SCHEMA は不在と同じ応答、未束縛はカタログ照会より前に `42501`、
/// 明示 Tx 内は `0A000`。
#[test]
fn tenant_boundary_and_gates() {
    let f = fx("sql40_dp_tenant");
    let mut alice = bound(ALICE, true);
    f.ok("t2", &mut bound(CAROL_T2, true), "CREATE SCHEMA theirs");
    // 他テナントにだけ在るロール（付与先・FOR ROLE）は不在と同じ `42704`。
    for sql in [
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO carol",
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO ghost",
        "ALTER DEFAULT PRIVILEGES FOR ROLE carol GRANT SELECT ON TABLES TO PUBLIC",
        "ALTER DEFAULT PRIVILEGES FOR ROLE ghost GRANT SELECT ON TABLES TO PUBLIC",
    ] {
        assert_eq!(f.code("t1", &mut alice, sql), "42704", "{sql}");
    }
    // 他テナントにだけ在る SCHEMA は不在と同じ `3F000`。
    for name in ["theirs", "nowhere"] {
        assert_eq!(
            f.code(
                "t1",
                &mut alice,
                &format!(
                    "ALTER DEFAULT PRIVILEGES IN SCHEMA {name} GRANT SELECT ON TABLES TO PUBLIC"
                )
            ),
            "3F000"
        );
    }
    // 未束縛は運用者権限の有無・対象の有無を問わず `42501`。
    for mut s in [SessionState::default(), operator()] {
        for sql in [
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO alice",
            "ALTER DEFAULT PRIVILEGES FOR ROLE ghost IN SCHEMA nowhere REVOKE ALL ON TABLES FROM ghost",
        ] {
            assert_eq!(f.code("t1", &mut s, sql), "42501", "{sql}");
        }
    }
    // 明示 Tx 内: 束縛済みは `0A000`、未束縛は `42501`。
    for (is_bound, expected) in [(true, "0A000"), (false, "42501")] {
        let mut s = SessionState::default();
        if is_bound {
            s.bind_role_ids(ALICE, ALICE);
        }
        let mut txn = f.core.new_session_transaction();
        f.core
            .execute_sql_in_txn(&ctx("t1"), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let e = f
            .core
            .execute_sql_in_txn(
                &ctx("t1"),
                &mut s,
                &mut txn,
                "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO bob",
            )
            .expect_err("rejected");
        assert_eq!(e.wire_code(), expected);
        f.core
            .execute_sql_in_txn(&ctx("t1"), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

/// t1 の既定権限は t2 の作成に作用しない。
#[test]
fn defaults_do_not_cross_tenants() {
    let f = fx("sql40_dp_cross");
    let mut alice = bound(ALICE, true);
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC",
    );
    let mut op = operator();
    f.ok("t2", &mut op, "CREATE ROLE dave");
    f.ok("t2", &mut bound(CAROL_T2, true), "CREATE TABLE x (a TEXT)");
    // dave（ID 4）は carol の表を読めない。
    assert_eq!(
        f.code("t2", &mut bound(4, false), "SELECT a FROM x"),
        "42501"
    );
}

/// 1 文の中の付与先が 1 つでも不在なら、何も保存しない。
#[test]
fn a_failing_statement_stores_nothing() {
    let f = fx("sql40_dp_atomic");
    let mut alice = bound(ALICE, true);
    let mut bob = bound(BOB, false);
    assert_eq!(
        f.code(
            "t1",
            &mut alice,
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO bob, ghost"
        ),
        "42704"
    );
    f.ok("t1", &mut alice, "CREATE TABLE t (a TEXT)");
    assert_eq!(f.code("t1", &mut bob, "SELECT a FROM t"), "42501");
}

/// `DROP ROLE` は既定権限の対象・付与先を `2BP01` で拒否する。`DROP SCHEMA` は当該 SCHEMA の既定権限を消す。
#[test]
fn drop_role_and_drop_schema_lifecycle() {
    let f = fx("sql40_dp_lifecycle");
    let mut alice = bound(ALICE, true);
    let mut op = operator();
    f.ok("t1", &mut alice, "CREATE SCHEMA s");
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES IN SCHEMA s GRANT SELECT ON TABLES TO bob",
    );
    assert_eq!(f.code("t1", &mut op, "DROP ROLE bob"), "2BP01");
    f.ok("t1", &mut alice, "DROP SCHEMA s");
    // スキーマ別の行だけが bob を参照していたので、SCHEMA の削除で依存が消える。
    f.ok("t1", &mut op, "DROP ROLE bob");
    // 対象ロール（alice）としての依存も拒否される。
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC",
    );
    assert_eq!(f.code("t1", &mut op, "DROP ROLE alice"), "2BP01");
    // 取り消して組み込みの既定へ戻せば依存も消える（alice は他に何も所有しない）。
    f.ok(
        "t1",
        &mut alice,
        "ALTER DEFAULT PRIVILEGES REVOKE SELECT ON TABLES FROM PUBLIC",
    );
    f.ok("t1", &mut op, "DROP ROLE alice");
}
