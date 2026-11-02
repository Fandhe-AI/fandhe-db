//! 読み取り系の文の SELECT 権限検査（SQL-40 (1)(2)(3)(6)・RLS-12 ポインタ。Issue #1967。設計は
//! `docs/design/role-privilege-model.md` §12）の結合テスト。
//!
//! 現在ロールを束縛したセッション（`SessionState::bind_role_ids`）で、`GRANT` で付与した表単位の
//! SELECT が JOIN・サブクエリ・CTE・集合演算・ビュー・EXPLAIN・カーソル・拡張クエリ・行ロック句・
//! HTTP 表層の bound 読み取り API に効くことを固定する。ロールを作る `Storage::create_role` が
//! `pub(crate)` のため crate 内に置く。未束縛のセッションが従来と同一に読めることも確認する。

use super::EngineCore;
use crate::catalog::role::{RoleAttributes, RoleDef};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::transaction::TransactionStatus;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const A: &str = "tenant-a";
const B: &str = "tenant-b";

struct Fx {
    core: EngineCore,
    _g: CleanupGuard,
    admin: u64,
    alice: u64,
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn role(storage: &Storage, tenant: &str, name: &str) -> u64 {
    storage
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

fn fx(label: &str) -> Fx {
    let path = unique_db_path(label);
    let g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    for name in ["ca", "cb", "cc"] {
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
    let admin = role(&storage, A, "admin_r");
    let alice = role(&storage, A, "alice");
    Fx {
        core: EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        _g: g,
        admin,
        alice,
    }
}

fn bound(id: u64, tenant: &str, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    assert!(s.bind_resolution_tenant(tenant));
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
        let mut s = bound(self.admin, A, true);
        self.core
            .execute_sql_in_session(&ctx(A), &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }

    fn alice_run(&self, sql: &str) -> String {
        let mut s = bound(self.alice, A, false);
        code(self.core.execute_sql_in_session(&ctx(A), &mut s, sql))
    }
}

#[test]
fn unbound_session_reads_without_any_grant() {
    let f = fx("selpriv-unbound");
    let mut s = SessionState::default();
    for sql in [
        "SELECT id FROM ca LIMIT 5",
        "SELECT ca.id FROM ca JOIN cb ON ca.id = cb.id LIMIT 5",
        "EXPLAIN SELECT id FROM ca LIMIT 5",
    ] {
        assert_eq!(
            code(f.core.execute_sql_in_session(&ctx(A), &mut s, sql)),
            "ok",
            "{sql}"
        );
    }
}

#[test]
fn bound_session_needs_select_on_the_table() {
    let f = fx("selpriv-basic");
    assert_eq!(f.alice_run("SELECT id FROM ca LIMIT 5"), "42501");
    f.admin_run("GRANT SELECT ON ca TO alice");
    assert_eq!(f.alice_run("SELECT id FROM ca LIMIT 5"), "ok");
    assert_eq!(f.alice_run("SELECT id FROM cb LIMIT 5"), "42501");
    // kNN の距離順（検索経路）も同じ検査を受ける。
    assert_eq!(
        f.alice_run("SELECT id FROM ca ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3"),
        "ok"
    );
    assert_eq!(
        f.alice_run("SELECT id FROM cb ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3"),
        "42501"
    );
}

#[test]
fn join_with_one_ungranted_side_is_denied() {
    let f = fx("selpriv-join");
    f.admin_run("GRANT SELECT ON ca TO alice");
    let join = "SELECT ca.id FROM ca JOIN cb ON ca.id = cb.id LIMIT 5";
    assert_eq!(f.alice_run(join), "42501");
    assert_eq!(
        f.alice_run("SELECT ca.id FROM ca LEFT JOIN cb ON ca.id = cb.id LIMIT 5"),
        "42501"
    );
    f.admin_run("GRANT SELECT ON cb TO alice");
    assert_eq!(f.alice_run(join), "ok");
}

#[test]
fn nested_references_are_checked() {
    let f = fx("selpriv-nested");
    f.admin_run("GRANT SELECT ON ca TO alice");
    for sql in [
        "SELECT id FROM ca WHERE id IN (SELECT id FROM cb) LIMIT 5",
        "SELECT id FROM ca WHERE EXISTS (SELECT 1 FROM cb WHERE cb.id = ca.id) LIMIT 5",
        "WITH c AS (SELECT id FROM cb) SELECT id FROM c LIMIT 5",
        "SELECT id FROM ca UNION SELECT id FROM cb",
        "SELECT id FROM (SELECT id FROM cb) AS d LIMIT 5",
    ] {
        assert_eq!(f.alice_run(sql), "42501", "{sql}");
    }
    f.admin_run("GRANT SELECT ON cb TO alice");
    assert_eq!(
        f.alice_run("SELECT id FROM ca WHERE id IN (SELECT id FROM cb) LIMIT 5"),
        "ok"
    );
}

#[test]
fn explain_is_checked_like_the_statement() {
    let f = fx("selpriv-explain");
    // `EXPLAIN ANALYZE` は構文として受理されない（`42601`）ため、計画を返す形は `EXPLAIN` だけ。
    let sql = "EXPLAIN SELECT id FROM ca LIMIT 5";
    let mut s = bound(f.alice, A, false);
    let e = f
        .core
        .execute_sql_in_session(&ctx(A), &mut s, sql)
        .expect_err("denied");
    assert_eq!(e.wire_code(), "42501", "{sql}");
    // 固定文言で、表名・計画の断片を含まない。
    let msg = e.client_message();
    assert!(!msg.contains("ca"), "{msg}");
    f.admin_run("GRANT SELECT ON ca TO alice");
    assert_eq!(f.alice_run("EXPLAIN SELECT id FROM ca LIMIT 5"), "ok");
}

#[test]
fn view_requires_the_view_privilege_only() {
    let f = fx("selpriv-view");
    f.admin_run("CREATE VIEW va AS SELECT id FROM ca");
    // 基底表だけに付与してもビュー経由では読めない。
    f.admin_run("GRANT SELECT ON ca TO alice");
    assert_eq!(f.alice_run("SELECT id FROM va LIMIT 5"), "42501");
    // ビューだけに付与すれば読める（基底表はビュー所有者の権限で読む扱い）。
    f.admin_run("REVOKE SELECT ON ca FROM alice");
    f.admin_run("GRANT SELECT ON va TO alice");
    assert_eq!(f.alice_run("SELECT id FROM va LIMIT 5"), "ok");
    assert_eq!(f.alice_run("SELECT id FROM ca LIMIT 5"), "42501");
}

#[test]
fn public_grant_and_revoke() {
    let f = fx("selpriv-public");
    f.admin_run("GRANT SELECT ON ca TO PUBLIC");
    assert_eq!(f.alice_run("SELECT id FROM ca LIMIT 5"), "ok");
    f.admin_run("REVOKE SELECT ON ca FROM PUBLIC");
    assert_eq!(f.alice_run("SELECT id FROM ca LIMIT 5"), "42501");
}

#[test]
fn owner_equivalent_session_reads_without_grants() {
    let f = fx("selpriv-owner");
    let mut s = bound(f.admin, A, true);
    assert_eq!(
        code(
            f.core
                .execute_sql_in_session(&ctx(A), &mut s, "SELECT id FROM ca LIMIT 5")
        ),
        "ok"
    );
}

#[test]
fn grants_do_not_cross_tenants() {
    let f = fx("selpriv-tenant");
    let bob = role(&f.core.storage, B, "bob");
    f.admin_run("GRANT SELECT ON ca TO PUBLIC");
    let mut s = bound(bob, B, false);
    assert_eq!(
        code(
            f.core
                .execute_sql_in_session(&ctx(B), &mut s, "SELECT id FROM ca LIMIT 5")
        ),
        "42501"
    );
}

#[test]
fn error_order_is_syntax_then_resolution_then_privilege() {
    let f = fx("selpriv-order");
    assert_eq!(f.alice_run("SELECT FROM FROM ca"), "42601");
    assert_eq!(f.alice_run("SELECT id FROM nosuch LIMIT 5"), "42P01");
    assert_eq!(f.alice_run("SELECT id FROM s_nowhere.ca LIMIT 5"), "3F000");
}

#[test]
fn column_level_grant_does_not_satisfy_table_level_read() {
    let f = fx("selpriv-column");
    f.admin_run("GRANT SELECT (n) ON ca TO alice");
    assert_eq!(f.alice_run("SELECT n FROM ca LIMIT 5"), "42501");
}

#[test]
fn temp_table_is_readable_without_grants() {
    let f = fx("selpriv-temp");
    let mut s = bound(f.alice, A, false);
    f.core
        .execute_sql_in_session(
            &ctx(A),
            &mut s,
            "CREATE TEMP TABLE tmp1 (id BIGINT PRIMARY KEY, n BIGINT)",
        )
        .expect("temp table");
    assert_eq!(
        code(
            f.core
                .execute_sql_in_session(&ctx(A), &mut s, "SELECT id FROM tmp1 LIMIT 5")
        ),
        "ok"
    );
}

#[test]
fn row_lock_needs_update_in_addition_to_select() {
    let f = fx("selpriv-lock");
    f.admin_run("GRANT SELECT ON ca TO alice");
    let mut s = bound(f.alice, A, false);
    let mut txn = f.core.new_session_transaction();
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let lock = "SELECT id FROM ca LIMIT 5 FOR UPDATE";
    let r = f.core.execute_sql_in_txn(&ctx(A), &mut s, &mut txn, lock);
    assert_eq!(code(r), "42501");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    f.admin_run("GRANT UPDATE ON ca TO alice");
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let r = f.core.execute_sql_in_txn(&ctx(A), &mut s, &mut txn, lock);
    assert_ne!(code(r), "42501");
}

#[test]
fn cursor_is_checked_at_declare_and_not_at_fetch() {
    let f = fx("selpriv-cursor");
    let mut s = bound(f.alice, A, false);
    let decl = "DECLARE c CURSOR FOR SELECT id FROM ca LIMIT 5";
    let mut txn = f.core.new_session_transaction();
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let r = f.core.execute_sql_in_txn(&ctx(A), &mut s, &mut txn, decl);
    assert_eq!(code(r), "42501");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");

    f.admin_run("GRANT SELECT ON ca TO alice");
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, decl)
        .expect("declare");
    // DECLARE の後の REVOKE は開いているカーソルに影響しない（結果は DECLARE の時点で確定する）。
    f.admin_run("REVOKE SELECT ON ca FROM alice");
    let r = f
        .core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "FETCH 1 FROM c");
    assert_eq!(code(r), "ok");
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
}

#[test]
fn extended_query_is_checked_at_parse_bind_and_describe() {
    let f = fx("selpriv-extended");
    let sql = "SELECT id FROM ca LIMIT 5";
    let s = bound(f.alice, A, false);
    // 付与なし: Parse の時点で拒否する。
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
    f.core
        .describe_prepared_in_session(&s, &prepared)
        .expect("describe");
    let txn = f.core.new_session_transaction();
    f.core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect("bind");
    // Parse 後の REVOKE は Bind と Describe で反映する。
    f.admin_run("REVOKE SELECT ON ca FROM alice");
    let e = f
        .core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect_err("denied at bind");
    assert_eq!(e.wire_code(), "42501");
    let e = f
        .core
        .describe_prepared_in_session(&s, &prepared)
        .expect_err("denied at describe");
    assert_eq!(e.wire_code(), "42501");
}

/// portal の Execute 直前の再検査: Bind 済みの文でも、その後の `REVOKE` と `SET ROLE` 相当の主体
/// 変更を反映して `42501` にする（Bind の結果を使い回さない）。
#[test]
fn portal_execute_rechecks_privileges_after_bind() {
    let f = fx("selpriv-portal-exec");
    let sql = "SELECT id FROM ca LIMIT 5";
    f.admin_run("GRANT SELECT ON ca TO alice");
    let s = bound(f.alice, A, false);
    let prepared = f
        .core
        .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
        .expect("parse");
    let txn = f.core.new_session_transaction();
    let parsed = f
        .core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect("bind");
    f.core
        .recheck_prepared_read_privileges_in_txn(&s, &txn, &prepared, &parsed)
        .expect("granted: execute allowed");
    // Bind 後の REVOKE は Execute 前の再検査で拒否する。
    f.admin_run("REVOKE SELECT ON ca FROM alice");
    let e = f
        .core
        .recheck_prepared_read_privileges_in_txn(&s, &txn, &prepared, &parsed)
        .expect_err("denied at execute");
    assert_eq!(e.wire_code(), "42501");
    // 現在ロールが変わった（付与の無いロール）セッションも拒否する。
    f.admin_run("GRANT SELECT ON ca TO alice");
    let other = bound(f.admin, A, false);
    let e = f
        .core
        .recheck_prepared_read_privileges_in_txn(&other, &txn, &prepared, &parsed)
        .expect_err("other principal denied");
    assert_eq!(e.wire_code(), "42501");
}

/// Bind 後に参照先が DROP されても、Execute 前の再検査は検査を省略せず拒否する（fail-closed。
/// 展開済みの文だけが残って、権限の無いロールが読めてしまう経路を作らない）。
#[test]
fn portal_execute_denies_when_referenced_relation_was_dropped() {
    let f = fx("selpriv-portal-dropped");
    let sql = "SELECT id FROM ca LIMIT 5";
    f.admin_run("GRANT SELECT ON ca TO alice");
    let s = bound(f.alice, A, false);
    let prepared = f
        .core
        .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
        .expect("parse");
    let txn = f.core.new_session_transaction();
    let parsed = f
        .core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect("bind");
    f.admin_run("DROP TABLE ca");
    let e = f
        .core
        .recheck_prepared_read_privileges_in_txn(&s, &txn, &prepared, &parsed)
        .expect_err("dropped relation must not skip the check");
    assert_eq!(e.wire_code(), "42P01");
}

/// SCHEMA 修飾の参照先が Bind 後に DROP されたときの再検査エラーは、内部 relation キー
/// （`#<schema_id>.<name>`）を含めず利用者向けの名前だけを返す（内部 ID の露出防止。ADR D2）。
#[test]
fn portal_execute_stale_schema_relation_error_hides_internal_key() {
    let f = fx("selpriv-portal-dropped-schema");
    f.admin_run("CREATE SCHEMA s");
    f.admin_run("CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT)");
    f.admin_run("GRANT USAGE ON SCHEMA s TO alice");
    f.admin_run("GRANT SELECT ON s.t TO alice");
    let s = bound(f.alice, A, false);
    let prepared = f
        .core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT id FROM s.t", &[])
        .expect("parse");
    let txn = f.core.new_session_transaction();
    let parsed = f
        .core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect("bind");
    f.admin_run("DROP TABLE s.t");
    let e = f
        .core
        .recheck_prepared_read_privileges_in_txn(&s, &txn, &prepared, &parsed)
        .expect_err("dropped relation must not skip the check");
    assert_eq!(e.wire_code(), "42P01");
    assert!(!e.client_message().contains('#'), "{}", e.client_message());
    assert!(e.client_message().contains('t'));
}

/// 未修飾名は `search_path` で SCHEMA 配下の表へは解決されない（`public` の裸の名前だけを引く）ため、
/// 同名の SCHEMA 表への権限は未修飾の参照に効かない。SELECT 権限の無いロールは、SCHEMA の USAGE が
/// あっても未修飾名で SCHEMA 表を読めない（存在しない扱いの `42P01`）。
#[test]
fn unqualified_name_never_reaches_schema_table() {
    let f = fx("selpriv-unqualified-schema");
    f.admin_run("CREATE SCHEMA s");
    f.admin_run("CREATE TABLE s.only_in_schema (id BIGINT PRIMARY KEY)");
    f.admin_run("GRANT USAGE ON SCHEMA s TO alice");
    assert_eq!(f.alice_run("SELECT id FROM only_in_schema"), "42P01");
    assert_eq!(f.alice_run("SELECT id FROM s.only_in_schema"), "42501");
}

#[test]
fn bound_http_read_api_is_checked_before_binding() {
    let f = fx("selpriv-http");
    let s = bound(f.alice, A, false);
    let never_bound = || SqlSurfaceError::invalid_input("binder reached");
    let r = f
        .core
        .execute_bound_scan_in_session(&ctx(A), &s, "ca", |_, _| Err(never_bound()));
    assert_eq!(r.expect_err("denied").wire_code(), "42501");
    let r = f
        .core
        .execute_bound_search_in_session(&ctx(A), &s, "ca", |_, _| Err(never_bound()));
    assert_eq!(r.expect_err("denied").wire_code(), "42501");
    let r = f
        .core
        .execute_bound_aggregate_in_session(&ctx(A), &s, "ca", |_, _| Err(never_bound()));
    assert_eq!(r.expect_err("denied").wire_code(), "42501");
    let r = f
        .core
        .explain_bound_scan_in_session(&ctx(A), &s, "ca", |_, _| Err(never_bound()));
    assert_eq!(r.expect_err("denied").wire_code(), "42501");
    f.admin_run("GRANT SELECT ON ca TO alice");
    let r = f
        .core
        .execute_bound_scan_in_session(&ctx(A), &s, "ca", |_, _| Err(never_bound()));
    // 権限があれば束縛まで進む（束縛クロージャのエラーが返る）。
    assert_ne!(r.expect_err("binder").wire_code(), "42501");
    // 未束縛のセッションは従来どおり束縛まで進む。
    let unbound = SessionState::default();
    let r =
        f.core
            .execute_bound_scan_in_session(&ctx(A), &unbound, "cb", |_, _| Err(never_bound()));
    assert_ne!(r.expect_err("binder").wire_code(), "42501");
}

/// BEGIN 後に別セッションが表を DROP しても、明示 Tx は BEGIN 時のスナップショットで解析・実行する。
/// 権限検査も同じスナップショットで引き、最新カタログで不在の表を検査省略にしない（fail-closed）。
#[test]
fn explicit_txn_check_uses_begin_snapshot_after_concurrent_drop() {
    let f = fx("selpriv-drop-after-begin");
    let mut s = bound(f.alice, A, false);
    let mut txn = f.core.new_session_transaction();
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    f.admin_run("DROP TABLE ca");
    let r = f
        .core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "SELECT id FROM ca LIMIT 5");
    assert_eq!(code(r), "42501");
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// 実表 `"only"` の別名 `x`（`ONLY` 直後の識別子）は、通常 SQL でも準備済み文でも関係名として扱われない。
/// 権限があれば両経路で成功し、権限が無ければ両経路で `42501`。Parse 時に実在を確認した実表が Parse 後に
/// DROP されたときだけ、Bind は `42P01` で拒否する（Issue #1967。規則は通常 SQL と同一）。
#[test]
fn alias_after_quoted_only_table_is_not_a_missing_relation_in_either_protocol() {
    let f = fx("selpriv-only-alias");
    f.admin_run("CREATE TABLE \"only\" (id BIGINT PRIMARY KEY)");
    let sql = "SELECT x.id FROM \"only\" x";
    let s = bound(f.alice, A, false);
    let mut simple = bound(f.alice, A, false);
    let denied = f
        .core
        .execute_sql_in_session(&ctx(A), &mut simple, sql)
        .expect_err("simple denied");
    assert_eq!(denied.wire_code(), "42501");
    let e = f
        .core
        .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
        .expect_err("prepared denied");
    assert_eq!(e.wire_code(), "42501");

    f.admin_run("GRANT SELECT ON \"only\" TO alice");
    f.core
        .execute_sql_in_session(&ctx(A), &mut simple, sql)
        .expect("simple granted");
    let prepared = f
        .core
        .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
        .expect("prepared granted");
    let txn = f.core.new_session_transaction();
    f.core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect("bind");
    f.admin_run("DROP TABLE \"only\"");
    let e = f
        .core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect_err("dropped after parse");
    assert_eq!(e.wire_code(), "42P01");
}

/// Bind 後にビューが DROP され、同名で別本文のビューが作られて GRANT されても、古い portal の
/// Execute 前の照合は失効として拒否する（名前の ACL だけを見ると、新しいビューの権限で古い本文＝別の表を
/// 読めてしまう。Issue #1967）。別の主体（`SET ROLE` 相当）でも同様。変更がなければ通る。
#[test]
fn replaced_view_invalidates_bound_portal_even_with_new_grant() {
    let f = fx("selpriv-view-replaced");
    f.admin_run("CREATE VIEW v AS SELECT n FROM ca");
    f.admin_run("GRANT SELECT ON v TO alice");
    let s = bound(f.alice, A, false);
    let prepared = f
        .core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT n FROM v", &[])
        .expect("parse");
    let txn = f.core.new_session_transaction();
    let identity = f
        .core
        .prepared_relation_identity_in_txn(&s, &txn, &prepared)
        .expect("identity");
    f.core
        .check_prepared_relation_identity(&s, &txn, &prepared, &identity)
        .expect("unchanged definitions stay valid");
    let parsed = f
        .core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect("bind");

    f.admin_run("DROP VIEW v");
    f.admin_run("CREATE VIEW v AS SELECT n FROM cb");
    f.admin_run("GRANT SELECT ON v TO alice");
    // 名前ベースの再検査は新しいビューの権限で通ってしまう（これだけでは足りない）。
    f.core
        .recheck_prepared_read_privileges_in_txn(&s, &txn, &prepared, &parsed)
        .expect("name-based recheck alone passes");
    let e = f
        .core
        .check_prepared_relation_identity(&s, &txn, &prepared, &identity)
        .expect_err("replaced view must invalidate the portal");
    assert_eq!(e.wire_code(), "0A000");
    // 現在ロールが変わった（新しいビューだけに権限を持つ）セッションでも同じ。
    f.admin_run("GRANT SELECT ON v TO admin_r");
    let other = bound(f.admin, A, false);
    let e = f
        .core
        .check_prepared_relation_identity(&other, &txn, &prepared, &identity)
        .expect_err("other principal also invalidated");
    assert_eq!(e.wire_code(), "0A000");
}

/// Parse 後に matview が DROP されると、Bind 後の再検査は誤分類として読み飛ばさず `42P01` で拒否する。
#[test]
fn dropped_materialized_view_is_rejected_after_parse() {
    let f = fx("selpriv-matview-dropped");
    f.admin_run("CREATE MATERIALIZED VIEW mv AS SELECT n FROM ca");
    f.admin_run("GRANT SELECT ON mv TO alice");
    let s = bound(f.alice, A, false);
    let prepared = f
        .core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT n FROM mv", &[])
        .expect("parse");
    let txn = f.core.new_session_transaction();
    let parsed = f
        .core
        .bind_prepared_in_txn(&s, &txn, &prepared, &[])
        .expect("bind");
    f.admin_run("DROP MATERIALIZED VIEW mv");
    let e = f
        .core
        .recheck_prepared_read_privileges_in_txn(&s, &txn, &prepared, &parsed)
        .expect_err("dropped matview must not skip the check");
    assert_eq!(e.wire_code(), "42P01");
}

/// INHERIT の閉包を SELECT の実効権限に反映する（Issue #1967・#2860）。INHERIT 真の辺（多段を含む）の先への
/// 付与で読め、INHERIT 偽の辺では読めず、`REVOKE` 後は読めない。
#[test]
fn select_follows_inherit_membership_closure() {
    let f = fx("selpriv-inherit");
    let q = "SELECT id FROM ca LIMIT 5";
    f.admin_run("CREATE ROLE readers");
    f.admin_run("CREATE ROLE mid");
    f.admin_run("GRANT SELECT ON ca TO readers");
    assert_eq!(f.alice_run(q), "42501");
    // INHERIT 偽の辺は権限を運ばない。
    f.admin_run("GRANT readers TO alice WITH INHERIT FALSE");
    assert_eq!(f.alice_run(q), "42501");
    f.admin_run("REVOKE readers FROM alice");
    f.admin_run("GRANT readers TO alice WITH INHERIT TRUE");
    assert_eq!(f.alice_run(q), "ok");
    f.admin_run("REVOKE readers FROM alice");
    assert_eq!(f.alice_run(q), "42501");
    // 多段（alice -> mid -> readers）。
    f.admin_run("GRANT readers TO mid WITH INHERIT TRUE");
    f.admin_run("GRANT mid TO alice WITH INHERIT TRUE");
    assert_eq!(f.alice_run(q), "ok");
    // 途中の辺が INHERIT 偽なら届かない。
    f.admin_run("REVOKE mid FROM alice");
    f.admin_run("GRANT mid TO alice WITH INHERIT FALSE");
    assert_eq!(f.alice_run(q), "42501");
}

/// 構文語と同名の実表（`"if"`・`"only"`・`"exists"`）は、暗黙別名・`AS` 別名・別名なしのどれで読んでも
/// 権限検査から外れない（通常 SQL と準備済み文の両方。権限なしは `42501`、権限ありは成功）。収集器の
/// 推測ではなく、解析が実在を確認した relation も検査対象に合成する（Issue #1967）。
#[test]
fn syntax_word_named_tables_are_gated_in_both_protocols() {
    let f = fx("selpriv-syntax-words");
    let s = bound(f.alice, A, false);
    let mut simple = bound(f.alice, A, false);
    for word in ["if", "only", "exists"] {
        f.admin_run(&format!("CREATE TABLE \"{word}\" (id BIGINT PRIMARY KEY)"));
        let queries = [
            format!("SELECT id FROM \"{word}\""),
            format!("SELECT x.id FROM \"{word}\" AS x"),
            format!("SELECT x.id FROM \"{word}\" x"),
            format!("SELECT id FROM \"{word}\" exists"),
        ];
        for q in &queries {
            // `exists` 自身を別名にする形は `"exists"` 表では意味が変わるため、構文が通るものだけを見る。
            let denied_simple = code(f.core.execute_sql_in_session(&ctx(A), &mut simple, q));
            if denied_simple == "42601" || denied_simple == "42P01" {
                continue;
            }
            assert_eq!(denied_simple, "42501", "simple: {q}");
            let e = f
                .core
                .parse_sql_prepared_with_declared_in_session(&s, q, &[])
                .expect_err("prepared denied");
            assert_eq!(e.wire_code(), "42501", "prepared: {q}");
        }
        f.admin_run(&format!("GRANT SELECT ON \"{word}\" TO alice"));
        for q in &queries {
            let granted = code(f.core.execute_sql_in_session(&ctx(A), &mut simple, q));
            if granted == "42601" || granted == "42P01" {
                continue;
            }
            assert_eq!(granted, "ok", "simple granted: {q}");
            f.core
                .parse_sql_prepared_with_declared_in_session(&s, q, &[])
                .unwrap_or_else(|e| panic!("prepared granted {q}: {e:?}"));
        }
    }
    // 対象の形が 1 件も通らずに空振りしていないこと。
    assert_eq!(
        code(
            f.core
                .execute_sql_in_session(&ctx(A), &mut simple, "SELECT id FROM \"if\" exists")
        ),
        "ok"
    );
}
