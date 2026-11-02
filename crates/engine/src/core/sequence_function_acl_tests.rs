//! SEQUENCE・関数単位の GRANT／REVOKE と権限検査の SQL 水準の単体テスト（SQL-40・RLS-12 のポインタ、
//! Issue #2235。設計記録: `docs/design/role-privilege-model.md`「Issue #2235 実装記録」）。
//!
//! `Storage::create_role` が crate 内限定のため、束縛ロールを要する経路をここで固定する。
//! - (A1) 権限の無い採番（`nextval`・`currval`・`setval`・`lastval`）と関数呼び出しが `42501`、拒否は副作用なし。
//! - (A2) SERIAL／IDENTITY 列（所有 SEQUENCE の DEFAULT）の暗黙の採番は SEQUENCE 権限なしで成功する。
//! - 判定順（不在の SEQUENCE は権限なしでも `42P01`・権限が値域より先）、未束縛・所有者相当は従来どおり、
//!   ライフサイクル（DROP で ACL 行と印が消える・`CREATE OR REPLACE` は引き継ぐ）、関数の既定（PUBLIC EXECUTE）。

use super::EngineCore;
use crate::catalog::privilege::PrivilegeObject;
use crate::catalog::role::{RoleAttributes, RoleDef};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn mk_role(core: &EngineCore, tenant: &str, name: &str) -> u64 {
    core.storage
        .create_role(
            tenant,
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

/// 束縛ロールのセッション。`owner` なら DDL 権限ゲート通過済み（所有者相当）。
fn bound(id: u64, owner: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    if owner {
        s.allow_ddl();
    }
    s
}

fn ok(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx("t1"), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()))
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> &'static str {
    match core.execute_sql_in_session(&ctx("t1"), s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn rows(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<Vec<Cell>> {
    match ok(core, s, sql) {
        SqlOutcome::Query(q) => q.rows.into_iter().map(|r| r.cells).collect(),
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

fn int(v: i64) -> Cell {
    Cell::SignedInteger(v)
}

/// owner（所有者相当）が SEQUENCE `q` と表 `t` を作り、非所有者 alice を返す。
fn fixture(tag: &str) -> (EngineCore, CleanupGuard, SessionState, SessionState) {
    let (core, g) = new_core(tag);
    let owner = mk_role(&core, "t1", "owner_r");
    let alice = mk_role(&core, "t1", "alice");
    let mut o = bound(owner, true);
    ok(&core, &mut o, "CREATE SEQUENCE q");
    ok(
        &core,
        &mut o,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT)",
    );
    (core, g, o, bound(alice, false))
}

fn ins_next(id: i64) -> String {
    format!("INSERT INTO t (id, n) VALUES ({id}, nextval('q'))")
}

fn ins_curr(id: i64) -> String {
    format!("INSERT INTO t (id, n) VALUES ({id}, currval('q'))")
}

/// (A1) 権限の無い採番はすべて `42501` で、値を消費せず行も増えない。
#[test]
fn sequence_functions_are_denied_without_privileges_and_leave_no_side_effect() {
    let (core, _g, mut o, mut a) = fixture("sfacl_seq_deny");
    assert_eq!(code(&core, &mut a, &ins_next(1)), "42501");
    assert_eq!(code(&core, &mut a, &ins_curr(1)), "42501");
    assert_eq!(code(&core, &mut a, "SELECT setval('q', 10)"), "42501");
    // 副作用なし: 行は無く、所有者の最初の nextval は 1（欠番なし）。setval も効いていない。
    assert!(rows(&core, &mut o, "SELECT id FROM t").is_empty());
    ok(&core, &mut o, &ins_next(1));
    assert_eq!(rows(&core, &mut o, "SELECT n FROM t"), vec![vec![int(1)]]);
}

#[test]
fn usage_update_select_each_open_only_their_functions() {
    let (core, _g, mut o, mut a) = fixture("sfacl_seq_bits");
    // USAGE: nextval・currval は通り、setval は拒否。
    ok(&core, &mut o, "GRANT USAGE ON SEQUENCE q TO alice");
    ok(&core, &mut a, &ins_next(1));
    ok(&core, &mut a, &ins_curr(2));
    assert_eq!(code(&core, &mut a, "SELECT setval('q', 10)"), "42501");
    ok(&core, &mut o, "REVOKE USAGE ON SEQUENCE q FROM alice");
    assert_eq!(code(&core, &mut a, &ins_next(3)), "42501");
    // UPDATE: nextval・setval は通り、currval は拒否。
    ok(&core, &mut o, "GRANT UPDATE ON SEQUENCE q TO alice");
    ok(&core, &mut a, &ins_next(3));
    ok(&core, &mut a, "SELECT setval('q', 10)");
    assert_eq!(code(&core, &mut a, &ins_curr(4)), "42501");
    ok(&core, &mut o, "REVOKE UPDATE ON SEQUENCE q FROM alice");
    // SELECT: currval・lastval は通り、nextval は拒否。
    ok(&core, &mut o, "GRANT SELECT ON SEQUENCE q TO alice");
    ok(&core, &mut a, &ins_curr(4));
    ok(&core, &mut a, "SELECT lastval()");
    assert_eq!(code(&core, &mut a, &ins_next(5)), "42501");
    // lastval は取り消されると拒否（対象は alice の最後の nextval の SEQUENCE）。
    ok(&core, &mut o, "REVOKE SELECT ON SEQUENCE q FROM alice");
    assert_eq!(code(&core, &mut a, "SELECT lastval()"), "42501");
}

/// 判定順: 不在は権限なしでも `42P01`、権限が値域（`22003`）・未定義（`55000`）より先。
#[test]
fn resolution_precedes_privilege_which_precedes_range_and_definedness() {
    let (core, _g, mut o, mut a) = fixture("sfacl_seq_order");
    let missing = "INSERT INTO t (id, n) VALUES (1, nextval('nosuch'))";
    assert_eq!(code(&core, &mut a, missing), "42P01");
    assert_eq!(code(&core, &mut a, "SELECT setval('nosuch', 1)"), "42P01");
    // 権限なしの currval は未定義（55000）より先に 42501。
    assert_eq!(code(&core, &mut a, &ins_curr(1)), "42501");
    assert_eq!(code(&core, &mut o, &ins_curr(1)), "55000");
    // 権限なしの setval は値域外（22003）より先に 42501。
    assert_eq!(code(&core, &mut a, "SELECT setval('q', 0)"), "42501");
    ok(&core, &mut o, "GRANT UPDATE ON SEQUENCE q TO alice");
    assert_eq!(code(&core, &mut a, "SELECT setval('q', 0)"), "22003");
}

/// (A2) SERIAL／IDENTITY 列の暗黙の採番は SEQUENCE 権限なしで通る。明示の `nextval`・所有されない
/// SEQUENCE の DEFAULT は検査される。
#[test]
fn implicit_owned_sequences_need_no_sequence_privilege() {
    let (core, _g, mut o, mut a) = fixture("sfacl_seq_implicit");
    ok(
        &core,
        &mut o,
        "CREATE TABLE ti (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v TEXT)",
    );
    ok(
        &core,
        &mut o,
        "CREATE TABLE ts (id BIGSERIAL PRIMARY KEY, v TEXT)",
    );
    ok(&core, &mut a, "INSERT INTO ti (v) VALUES ('a')");
    ok(&core, &mut a, "INSERT INTO ti DEFAULT VALUES");
    ok(&core, &mut a, "INSERT INTO ts (v) VALUES ('a')");
    ok(&core, &mut a, "INSERT INTO ts (v) VALUES ('b')");
    assert_eq!(
        rows(&core, &mut o, "SELECT id FROM ts ORDER BY id"),
        vec![vec![int(1)], vec![int(2)]]
    );
    // 暗黙 SEQUENCE の明示呼び出しは検査される。
    assert_eq!(
        code(
            &core,
            &mut a,
            "INSERT INTO ts (id, v) VALUES (nextval('ts_id_seq'), 'x')"
        ),
        "42501"
    );
    // 所有されていない SEQUENCE を参照する利用者の DEFAULT は検査される。
    ok(&core, &mut o, "CREATE SEQUENCE u");
    ok(
        &core,
        &mut o,
        "CREATE TABLE td (n BIGINT DEFAULT nextval('u'), tag TEXT)",
    );
    assert_eq!(
        code(&core, &mut a, "INSERT INTO td (tag) VALUES ('x')"),
        "42501"
    );
    ok(&core, &mut o, "GRANT USAGE ON SEQUENCE u TO alice");
    ok(&core, &mut a, "INSERT INTO td (tag) VALUES ('x')");
}

#[test]
fn unbound_and_owner_equivalent_sessions_are_not_checked() {
    let (core, _g, mut o, _a) = fixture("sfacl_seq_unbound");
    ok(&core, &mut o, &ins_next(1));
    ok(&core, &mut o, "SELECT setval('q', 10)");
    ok(&core, &mut o, "SELECT lastval()");
    let mut unbound = SessionState::default();
    ok(&core, &mut unbound, &ins_next(2));
    ok(&core, &mut unbound, "SELECT setval('q', 20)");
}

/// 構文・対象の解決（不在 `42P01`・表を指すと `42809`・PUBLIC・CASCADE）と、付与された行の中身。
#[test]
fn grant_on_sequence_resolves_and_records_rows() {
    let (core, _g, mut o, mut a) = fixture("sfacl_seq_grant");
    assert_eq!(
        code(&core, &mut o, "GRANT USAGE ON SEQUENCE nosuch TO alice"),
        "42P01"
    );
    assert_eq!(
        code(&core, &mut o, "GRANT USAGE ON SEQUENCE t TO alice"),
        "42809"
    );
    assert_eq!(
        code(&core, &mut o, "GRANT INSERT ON SEQUENCE q TO alice"),
        "0LP01"
    );
    // 未束縛の GRANT は対象の有無によらず 42501。
    let mut unbound = SessionState::default();
    assert_eq!(
        code(
            &core,
            &mut unbound,
            "GRANT USAGE ON SEQUENCE nosuch TO alice"
        ),
        "42501"
    );
    ok(&core, &mut o, "GRANT ALL ON SEQUENCE q TO PUBLIC");
    ok(&core, &mut a, &ins_next(1));
    ok(&core, &mut a, "SELECT setval('q', 5)");
    ok(&core, &mut o, "REVOKE ALL ON SEQUENCE q FROM PUBLIC");
    assert_eq!(code(&core, &mut a, &ins_next(2)), "42501");
    // 他テナントの同名 SEQUENCE は ACL 行を共有しない（行は tenant_id 先頭キー）。
    let id = core
        .storage
        .list_acl_for_object("t2", &PrivilegeObject::Sequence(1))
        .expect("acl");
    assert!(id.is_empty());
}

/// DROP SEQUENCE・所有表の DROP（SERIAL の道連れ）で ACL 行が消え、付与先ロールの `DROP ROLE` を塞がない。
#[test]
fn dropping_a_sequence_removes_its_acl_rows() {
    use redb::ReadableDatabase;
    let (core, _g, mut o, _a) = fixture("sfacl_seq_drop");
    let alice = core
        .storage
        .get_role("t1", "alice")
        .expect("role")
        .expect("some")
        .id;
    ok(&core, &mut o, "CREATE SEQUENCE z");
    ok(&core, &mut o, "GRANT USAGE ON SEQUENCE z TO alice");
    let referenced = |core: &EngineCore| {
        let rt = core.storage.db().begin_read().expect("rt");
        crate::catalog::privilege::acl_references_role_in_txn(&rt, "t1", alice).expect("ref")
    };
    assert!(referenced(&core));
    ok(&core, &mut o, "DROP SEQUENCE z");
    assert!(!referenced(&core));
    // SERIAL の所有 SEQUENCE は表の DROP の道連れ。
    ok(
        &core,
        &mut o,
        "CREATE TABLE ts (id BIGSERIAL PRIMARY KEY, v TEXT)",
    );
    ok(&core, &mut o, "GRANT SELECT ON SEQUENCE ts_id_seq TO alice");
    assert!(referenced(&core));
    ok(&core, &mut o, "DROP TABLE ts");
    assert!(!referenced(&core));
}

/// owner が宣言的 UDF・`LANGUAGE sql` の関数・集合返却関数を作り、alice の呼び出し専用セッションを返す。
fn fn_fixture(tag: &str) -> (EngineCore, CleanupGuard, SessionState) {
    let (core, g) = new_core(tag);
    let owner = mk_role(&core, "t1", "owner_r");
    mk_role(&core, "t1", "alice");
    let mut o = bound(owner, true);
    ok(
        &core,
        &mut o,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT)",
    );
    ok(&core, &mut o, "INSERT INTO t (id, n) VALUES (1, 10)");
    ok(&core, &mut o, "CREATE FUNCTION addone(x) AS x + 1");
    ok(&core, &mut o, "CREATE FUNCTION outer_f(x) AS addone(x) + 1");
    ok(
        &core,
        &mut o,
        "CREATE FUNCTION r() RETURNS int LANGUAGE sql AS $$ SELECT 7 $$",
    );
    ok(
        &core,
        &mut o,
        "CREATE FUNCTION sr() RETURNS SETOF int LANGUAGE sql AS $$ SELECT 1 $$",
    );
    let alice = core
        .storage
        .get_role("t1", "alice")
        .expect("role")
        .expect("some")
        .id;
    let mut a = bound(alice, false);
    core.load_session_udfs(&ctx("t1"), &mut a).expect("load");
    (core, g, a)
}

fn owner_session(core: &EngineCore) -> SessionState {
    let id = core
        .storage
        .get_role("t1", "owner_r")
        .expect("role")
        .expect("some")
        .id;
    let mut o = bound(id, true);
    core.load_session_udfs(&ctx("t1"), &mut o).expect("load");
    o
}

/// 新規の関数は PUBLIC が EXECUTE を持つ（既定）。REVOKE FROM PUBLIC で拒否、GRANT で復帰する。
#[test]
fn function_execute_defaults_to_public_and_follows_revoke_and_grant() {
    let (core, _g, mut a) = fn_fixture("sfacl_fn_basic");
    let mut o = owner_session(&core);
    let calls = [
        "SELECT addone(n) FROM t",
        "SELECT id FROM t WHERE addone(n) = 11",
        "SELECT outer_f(n) FROM t",
        "SELECT r()",
        "SELECT * FROM sr()",
    ];
    for sql in calls {
        ok(&core, &mut a, sql);
    }
    for f in ["addone", "r", "sr"] {
        ok(
            &core,
            &mut o,
            &format!("REVOKE EXECUTE ON FUNCTION {f} FROM PUBLIC"),
        );
    }
    for sql in calls {
        assert_eq!(code(&core, &mut a, sql), "42501", "{sql}");
    }
    // 所有者相当は影響を受けない。
    ok(&core, &mut o, "SELECT addone(n) FROM t");
    // 入れ子（outer_f の本体経由）の呼び出しは呼び出し元ロールで判定される: addone だけ付与すると通る。
    ok(&core, &mut o, "GRANT EXECUTE ON FUNCTION addone TO alice");
    ok(&core, &mut a, "SELECT addone(n) FROM t");
    ok(&core, &mut o, "GRANT EXECUTE ON FUNCTION r() TO PUBLIC");
    ok(&core, &mut a, "SELECT r()");
    assert_eq!(code(&core, &mut a, "SELECT * FROM sr()"), "42501");
    ok(
        &core,
        &mut o,
        "REVOKE EXECUTE ON FUNCTION addone FROM alice",
    );
    assert_eq!(code(&core, &mut a, "SELECT addone(n) FROM t"), "42501");
}

#[test]
fn function_execute_is_checked_in_explicit_transactions_too() {
    let (core, _g, mut a) = fn_fixture("sfacl_fn_txn");
    let mut o = owner_session(&core);
    ok(
        &core,
        &mut o,
        "REVOKE EXECUTE ON FUNCTION addone FROM PUBLIC",
    );
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx("t1"), &mut a, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(&ctx("t1"), &mut a, &mut txn, "SELECT addone(n) FROM t")
        .expect_err("denied");
    assert_eq!(e.wire_code(), "42501");
}

/// 判定順: 不在の関数は権限に関わらず `42883`、他テナントの関数と区別できない。GRANT の対象解決も同じ。
#[test]
fn missing_function_is_indistinguishable_and_grant_resolves_signatures() {
    let (core, _g, _a) = fn_fixture("sfacl_fn_resolve");
    let mut o = owner_session(&core);
    for sql in [
        "GRANT EXECUTE ON FUNCTION nosuch TO alice",
        "GRANT EXECUTE ON FUNCTION addone(text, text) TO alice",
        "REVOKE EXECUTE ON FUNCTION nosuch() FROM alice",
    ] {
        assert_eq!(code(&core, &mut o, sql), "42883", "{sql}");
    }
    // 他テナントのセッションから見ても同じ（t1 の関数は引けない）。
    let mut other = SessionState::default();
    other.allow_ddl();
    other.bind_role_ids(1, 1);
    let e = core
        .execute_sql_in_session(
            &ctx("t2"),
            &mut other,
            "GRANT EXECUTE ON FUNCTION addone TO PUBLIC",
        )
        .expect_err("no such function in t2");
    assert!(matches!(e.wire_code(), "42883" | "42501"));
    ok(
        &core,
        &mut o,
        "GRANT EXECUTE ON FUNCTION addone(integer) TO alice",
    );
    assert_eq!(
        code(&core, &mut o, "GRANT USAGE ON FUNCTION addone TO alice"),
        "0LP01"
    );
    assert_eq!(
        code(&core, &mut o, "GRANT EXECUTE ON PROCEDURE p TO alice"),
        "0A000"
    );
}

/// 付与権限を持たない束縛ロールの GRANT は変更せず（警告のみ）、REVOKE FROM PUBLIC の後も `GRANT ... TO
/// PUBLIC` で既定へ戻せる。DROP FUNCTION で行と印が消え、CREATE OR REPLACE は ACL を引き継ぐ。
#[test]
fn function_acl_lifecycle() {
    let (core, _g, mut a) = fn_fixture("sfacl_fn_life");
    let mut o = owner_session(&core);
    // 付与権限（GRANT OPTION）を持たない alice の GRANT／REVOKE は何も変えない。
    ok(
        &core,
        &mut a,
        "REVOKE EXECUTE ON FUNCTION addone FROM PUBLIC",
    );
    ok(&core, &mut a, "SELECT addone(n) FROM t");
    ok(
        &core,
        &mut o,
        "REVOKE EXECUTE ON FUNCTION addone FROM PUBLIC",
    );
    assert_eq!(code(&core, &mut a, "SELECT addone(n) FROM t"), "42501");
    // CREATE OR REPLACE は内部 ID を保つので ACL を引き継ぐ。
    ok(
        &core,
        &mut o,
        "CREATE OR REPLACE FUNCTION addone(x) AS x + 1",
    );
    core.load_session_udfs(&ctx("t1"), &mut a).expect("reload");
    assert_eq!(code(&core, &mut a, "SELECT addone(n) FROM t"), "42501");
    ok(&core, &mut o, "GRANT EXECUTE ON FUNCTION addone TO PUBLIC");
    ok(&core, &mut a, "SELECT addone(n) FROM t");
    // DROP FUNCTION で ACL 行と印が消える（同名の再作成は新しい ID で既定の PUBLIC EXECUTE）。
    let id = crate::catalog::function::resolve_function_id_in_txn(
        &{
            use redb::ReadableDatabase;
            core.storage.db().begin_read().expect("rt")
        },
        "t1",
        "addone",
        None,
    )
    .expect("resolve")
    .expect("some");
    ok(
        &core,
        &mut o,
        "REVOKE EXECUTE ON FUNCTION addone FROM PUBLIC",
    );
    ok(&core, &mut o, "DROP FUNCTION addone CASCADE");
    assert!(core
        .storage
        .list_acl_for_object("t1", &PrivilegeObject::Function(id))
        .expect("acl")
        .is_empty());
    assert_eq!(
        core.storage.function_privileges("t1", id, &[1]).expect("p"),
        crate::catalog::privilege::PRIV_EXECUTE
    );
}
