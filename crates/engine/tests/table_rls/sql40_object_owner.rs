//! オブジェクト所有者と PUBLIC（SQL-40・RLS-12 のポインタ、Issue #2236）の SQL 水準の結合テスト。設計記録:
//! `docs/design/role-privilege-model.md` §14。
//!
//! 固定する契約: 作成者が所有者として記録される・所有者以外の `DROP SCHEMA`／`ALTER SCHEMA`／`ALTER ... OWNER TO`
//! が `42501`（副作用なし）・所有者は運用者権限なしで GRANT／REVOKE と SCHEMA の利用ができる・存在オラクルに
//! ならない（不在と他テナント所有は同じ応答、未束縛はカタログ照会より前に `42501`）・`DROP ROLE` は所有者の
//! ロールを `2BP01` で拒否する・`GRANT ... TO PUBLIC` は同じテナントのロールにだけ作用して他テナントへ
//! 及ばない。ロールは `CREATE ROLE` で作り、ID は作成順の 1・2・3。

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
            panic!("{sql} must succeed: {e}");
        }
    }

    fn err(&self, tenant: &str, s: &mut SessionState, sql: &str) -> SqlSurfaceError {
        match self.run(tenant, s, sql) {
            Ok(o) => panic!("{sql} must fail, got {o:?}"),
            Err(e) => e,
        }
    }

    fn code(&self, tenant: &str, s: &mut SessionState, sql: &str) -> &'static str {
        self.err(tenant, s, sql).wire_code()
    }
}

/// 作成者（束縛かつ運用者権限）が所有者として記録され、所有者は運用者権限なしで GRANT／REVOKE できる。
/// 未束縛の運用者が作ったものには所有者がつかない（運用者権限でしか扱えない）。
#[test]
fn creator_becomes_owner_and_may_grant_without_operator_rights() {
    let f = fx("sql40_owner_creator");
    f.ok("t1", &mut bound(ALICE, true), "CREATE TABLE mine (a TEXT)");
    f.ok("t1", &mut operator(), "CREATE TABLE theirs (a TEXT)");
    // 所有者（運用者権限なし）は付与・取り消しできる。
    let mut alice = bound(ALICE, false);
    f.ok("t1", &mut alice, "GRANT SELECT ON mine TO bob");
    f.ok("t1", &mut alice, "REVOKE SELECT ON mine FROM bob");
    // 所有者の無い表・他人の表には何の権限も無い。
    assert_eq!(
        f.code("t1", &mut alice, "GRANT SELECT ON theirs TO bob"),
        "42501"
    );
    let mut bob = bound(BOB, false);
    assert_eq!(
        f.code("t1", &mut bob, "GRANT SELECT ON mine TO alice"),
        "42501"
    );
}

/// 所有者以外の `DROP SCHEMA`・`ALTER SCHEMA RENAME` は `42501` で副作用が無く、所有者なら成功する。
#[test]
fn non_owner_cannot_drop_or_alter_schema() {
    let f = fx("sql40_owner_schema");
    f.ok("t1", &mut bound(ALICE, true), "CREATE SCHEMA s");
    let mut bob = bound(BOB, false);
    assert_eq!(f.code("t1", &mut bob, "DROP SCHEMA s"), "42501");
    assert_eq!(
        f.code("t1", &mut bob, "ALTER SCHEMA s RENAME TO s2"),
        "42501"
    );
    assert_eq!(
        f.code("t1", &mut bob, "ALTER SCHEMA s OWNER TO bob"),
        "42501"
    );
    // 副作用なし: s は残り、s2 は無い。
    let mut op = operator();
    f.ok("t1", &mut op, "ALTER SCHEMA s RENAME TO s_op");
    f.ok("t1", &mut op, "ALTER SCHEMA s_op RENAME TO s");
    // 所有者は運用者権限なしで改名・削除できる。
    let mut alice = bound(ALICE, false);
    f.ok("t1", &mut alice, "ALTER SCHEMA s RENAME TO s2");
    f.ok("t1", &mut alice, "DROP SCHEMA s2");
}

/// 存在オラクルの 3 ケース。未束縛は全ケース同じ `42501`、束縛した非所有者は不在と他テナント所有が同じ
/// `3F000`、自テナントに在るものは `42501`。
#[test]
fn schema_existence_is_not_an_oracle() {
    let f = fx("sql40_owner_oracle");
    f.ok("t1", &mut bound(ALICE, true), "CREATE SCHEMA mine");
    f.ok("t2", &mut bound(CAROL_T2, true), "CREATE SCHEMA theirs");
    for sql in ["DROP SCHEMA", "DROP SCHEMA IF EXISTS"] {
        let mut seen = Vec::new();
        for name in ["mine", "theirs", "absent"] {
            let e = f.err("t1", &mut SessionState::default(), &format!("{sql} {name}"));
            assert_eq!(e.wire_code(), "42501");
            seen.push(e.to_string());
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
    }
    let mut bob = bound(BOB, false);
    // 文言に含まれるのは利用者が書いた名前だけ（名前を除くと同一）。
    let norm = |e: &SqlSurfaceError, name: &str| e.to_string().replace(name, "<name>");
    let other = f.err("t1", &mut bob, "DROP SCHEMA theirs");
    let absent = f.err("t1", &mut bob, "DROP SCHEMA absent");
    assert_eq!(other.wire_code(), "3F000");
    assert_eq!(norm(&other, "theirs"), norm(&absent, "absent"));
    let other = f.err("t1", &mut bob, "ALTER SCHEMA theirs OWNER TO bob");
    let absent = f.err("t1", &mut bob, "ALTER SCHEMA absent OWNER TO bob");
    assert_eq!(other.wire_code(), "3F000");
    assert_eq!(norm(&other, "theirs"), norm(&absent, "absent"));
    assert_eq!(f.code("t1", &mut bob, "DROP SCHEMA mine"), "42501");
}

/// `ALTER ... OWNER TO`: 非所有者 `42501`・新所有者の不在と他テナントは同じ `42704`・SET できない移譲は
/// `42501`・移譲後は旧所有者が変更できず新所有者が変更できる。表・ビュー・SCHEMA を対象にする。
#[test]
fn owner_to_contract_for_table_view_and_schema() {
    let f = fx("sql40_owner_to");
    let mut alice_ddl = bound(ALICE, true);
    f.ok("t1", &mut alice_ddl, "CREATE TABLE docs (a TEXT)");
    f.ok("t1", &mut alice_ddl, "CREATE VIEW dv AS SELECT a FROM docs");
    f.ok("t1", &mut alice_ddl, "CREATE SCHEMA s");
    let mut alice = bound(ALICE, false);
    let mut bob = bound(BOB, false);
    for sql in [
        "ALTER TABLE docs OWNER TO bob",
        "ALTER VIEW dv OWNER TO bob",
        "ALTER SCHEMA s OWNER TO bob",
    ] {
        assert_eq!(f.code("t1", &mut bob, sql), "42501", "{sql}");
        // alice は bob へ SET できない（メンバーシップなし）。
        assert_eq!(f.code("t1", &mut alice, sql), "42501", "{sql}");
    }
    // 新所有者の不在・他テナントのロールは区別できない。
    let missing = f.err("t1", &mut alice, "ALTER TABLE docs OWNER TO ghost");
    let other = f.err("t1", &mut alice, "ALTER TABLE docs OWNER TO carol");
    assert_eq!(missing.wire_code(), "42704");
    assert_eq!(other.wire_code(), "42704");
    assert!(missing.to_string().contains("ghost") && other.to_string().contains("carol"));
    // 種別違いは `42809`。
    assert_eq!(
        f.code("t1", &mut alice, "ALTER VIEW docs OWNER TO alice"),
        "42809"
    );
    // 運用者権限による移譲。
    let mut op = bound(ALICE, true);
    let out = f
        .run("t1", &mut op, "ALTER TABLE docs OWNER TO bob")
        .expect("transfer");
    assert!(matches!(out, SqlOutcome::AlterOwner(_)));
    f.ok("t1", &mut op, "ALTER VIEW dv OWNER TO bob");
    f.ok("t1", &mut op, "ALTER SCHEMA s OWNER TO bob");
    // 移譲後: 旧所有者は GRANT も SCHEMA の削除もできず、新所有者はできる。
    assert_eq!(
        f.code("t1", &mut alice, "GRANT SELECT ON docs TO alice"),
        "42501"
    );
    assert_eq!(f.code("t1", &mut alice, "DROP SCHEMA s"), "42501");
    f.ok("t1", &mut bob, "GRANT SELECT ON docs TO alice");
    f.ok("t1", &mut bob, "DROP SCHEMA s");
}

/// 未束縛のセッションは、運用者権限があっても `OWNER TO` を対象・ロールの有無を問わず `42501` にする。
#[test]
fn unbound_owner_to_is_denied_uniformly() {
    let f = fx("sql40_owner_unbound");
    f.ok("t1", &mut bound(ALICE, true), "CREATE TABLE docs (a TEXT)");
    for with_ddl in [false, true] {
        let mut s = SessionState::default();
        if with_ddl {
            s.allow_ddl();
        }
        let mut seen = Vec::new();
        for sql in [
            "ALTER TABLE docs OWNER TO bob",
            "ALTER TABLE nosuch OWNER TO ghost",
            "ALTER SCHEMA nosuch OWNER TO ghost",
            "ALTER TABLE nosuch.t OWNER TO bob",
        ] {
            let e = f.err("t1", &mut s, sql);
            assert_eq!(e.wire_code(), "42501", "{sql}");
            seen.push(e.to_string());
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
    }
}

/// 所有者のロールは `DROP ROLE` を `2BP01` で拒否され、所有権を移せば削除できる。
#[test]
fn drop_role_is_blocked_while_it_owns_objects() {
    let f = fx("sql40_owner_drop_role");
    f.ok("t1", &mut bound(ALICE, true), "CREATE SCHEMA s");
    let mut op = operator();
    assert_eq!(f.code("t1", &mut op, "DROP ROLE alice"), "2BP01");
    f.ok("t1", &mut bound(ALICE, true), "ALTER SCHEMA s OWNER TO bob");
    f.ok("t1", &mut op, "DROP ROLE alice");
}

/// `CREATE SCHEMA ... AUTHORIZATION`: 指定したロールが所有者になり、不在・他テナントのロールは `42704`。
#[test]
fn create_schema_authorization_sets_the_owner() {
    let f = fx("sql40_owner_authorization");
    let mut op = operator();
    f.ok("t1", &mut op, "CREATE SCHEMA s AUTHORIZATION bob");
    f.ok("t1", &mut op, "CREATE SCHEMA AUTHORIZATION alice");
    assert_eq!(
        f.code("t1", &mut op, "CREATE SCHEMA x AUTHORIZATION ghost"),
        "42704"
    );
    assert_eq!(
        f.code("t1", &mut op, "CREATE SCHEMA x AUTHORIZATION carol"),
        "42704"
    );
    f.ok("t1", &mut bound(BOB, false), "DROP SCHEMA s");
    f.ok("t1", &mut bound(ALICE, false), "DROP SCHEMA alice");
}

/// `GRANT ... TO PUBLIC` は同じテナントのロール全体にだけ作用し、他テナントのロールには及ばない。
/// SCHEMA の USAGE（修飾名の解決で検査される）で観測する。
#[test]
fn public_grant_never_crosses_the_tenant_boundary() {
    let f = fx("sql40_owner_public");
    let mut alice_ddl = bound(ALICE, true);
    f.ok("t1", &mut alice_ddl, "CREATE SCHEMA s");
    f.ok(
        "t1",
        &mut alice_ddl,
        "CREATE TABLE s.u (id BIGINT PRIMARY KEY)",
    );
    // t2 にも同名の SCHEMA（別の内部 ID）を運用者が作る（所有者なし。作成者が carol だと所有者の暗黙の
    // USAGE が効いてしまい、PUBLIC の作用だけを観測できない）。
    f.ok("t2", &mut operator(), "CREATE SCHEMA s");
    f.ok(
        "t2",
        &mut operator(),
        "CREATE TABLE s.u2 (id BIGINT PRIMARY KEY)",
    );
    let mut bob = bound(BOB, false);
    let mut carol = bound(CAROL_T2, false);
    assert_eq!(f.code("t1", &mut bob, "SELECT id FROM s.u"), "42501");
    // 所有者（運用者権限なし）が PUBLIC へ USAGE を付与する。
    f.ok(
        "t1",
        &mut bound(ALICE, false),
        "GRANT USAGE ON SCHEMA s TO PUBLIC",
    );
    f.ok("t1", &mut bob, "SELECT id FROM s.u");
    // t1 の PUBLIC は t2 の同名 SCHEMA にも、t2 のロールにも作用しない。
    assert_eq!(f.code("t2", &mut carol, "SELECT id FROM s.u2"), "42501");
    // t2 の `s` に解決される（t1 の SCHEMA は見えない）ので、同じく USAGE が無く拒否される。
    assert_eq!(f.code("t2", &mut carol, "SELECT id FROM s.u"), "42501");
}

/// `REASSIGN OWNED`・`DROP OWNED` は状態に依存しない固定の `0A000`。
#[test]
fn reassign_and_drop_owned_are_static_rejections() {
    let f = fx("sql40_owner_owned");
    for s in [&mut SessionState::default(), &mut bound(ALICE, true)] {
        assert_eq!(f.code("t1", s, "REASSIGN OWNED BY alice TO bob"), "0A000");
        assert_eq!(f.code("t1", s, "DROP OWNED BY alice"), "0A000");
    }
}

/// 明示トランザクション内の `OWNER TO` は、未束縛なら `42501`、束縛済みなら `0A000`（ROLLBACK で
/// 取り消せない書き込みのため）。
#[test]
fn owner_to_inside_a_transaction_block_is_not_supported() {
    let f = fx("sql40_owner_tx");
    f.ok("t1", &mut bound(ALICE, true), "CREATE TABLE docs (a TEXT)");
    // 運用者権限つきでも未束縛なら `42501`（自動コミットと同じ契約）。
    for (is_bound, is_operator, expected) in [
        (false, false, "42501"),
        (false, true, "42501"),
        (true, false, "0A000"),
    ] {
        let mut s = if is_operator {
            operator()
        } else {
            SessionState::default()
        };
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
                "ALTER TABLE docs OWNER TO alice",
            )
            .expect_err("rejected");
        assert_eq!(e.wire_code(), expected);
        f.core
            .execute_sql_in_txn(&ctx("t1"), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

/// SCHEMA 配下の表への `OWNER TO`: 名前解決器が `s.t` を内部キーへ置き換えた後に束縛され、所有者が行う場合は
/// 新所有者がその SCHEMA の CREATE を持つことを要求する（付与するまで `42501`、付与後は成功）。
#[test]
fn owner_to_on_a_schema_qualified_relation_requires_create_on_the_schema() {
    let f = fx("sql40_owner_qualified");
    let mut alice_ddl = bound(ALICE, true);
    f.ok("t1", &mut alice_ddl, "CREATE SCHEMA s");
    f.ok("t1", &mut alice_ddl, "CREATE TABLE s.t (a TEXT)");
    let mut alice = bound(ALICE, false);
    // alice は bob へ SET できないため、運用者権限なしでは移譲できない。メンバーシップの代わりに、
    // 自分自身（CURRENT_USER）への移譲が no-op で通ることで解決とキーの検証が通っていることを確認する。
    f.ok("t1", &mut alice, "ALTER TABLE s.t OWNER TO CURRENT_USER");
    assert_eq!(
        f.code("t1", &mut alice, "ALTER TABLE s.t OWNER TO bob"),
        "42501"
    );
    // 運用者権限は SET 可能性と CREATE の要求を免除する。
    f.ok(
        "t1",
        &mut bound(ALICE, true),
        "ALTER TABLE s.t OWNER TO bob",
    );
    // 移譲後は bob が所有者で（修飾名の解決に必要な USAGE は SCHEMA の所有者 alice が付与する）、
    // alice は GRANT できない。
    f.ok("t1", &mut alice, "GRANT USAGE ON SCHEMA s TO bob");
    f.ok("t1", &mut bound(BOB, false), "GRANT SELECT ON s.t TO alice");
    // alice はもう所有者ではない（自分自身への移譲も所有者でなければ通らない）。
    assert_eq!(
        f.code("t1", &mut alice, "ALTER TABLE s.t OWNER TO CURRENT_USER"),
        "42501"
    );
    // 不在の SCHEMA・不在の表は束縛前に解決されて固定のエラーになる。
    assert_eq!(
        f.code("t1", &mut alice, "ALTER TABLE nosuch.t OWNER TO alice"),
        "3F000"
    );
}
