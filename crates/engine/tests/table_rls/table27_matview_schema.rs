//! MATERIALIZED VIEW の SCHEMA 配置（TABLE-27・TABLE-42・Issue #3287）の結合テスト。
//! ポインタ: TABLE-27・TABLE-44・SQL-39・SQL-40・RLS-7・RLS-8・RLS-10。設計記録は
//! `docs/design/schema-tenant-namespace.md`（D2・D5・D7・D8、追記 Issue #3287）。
//!
//! `table27_schema_objects.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - `CREATE`／`REFRESH`／`SELECT`／`DROP MATERIALIZED VIEW` が修飾名でも `search_path` 経由の未修飾名でも通る
//! - 同名の `public` の matview と独立している（行・REFRESH・DROP が混ざらない）
//! - `DROP SCHEMA` は RESTRICT で `2BP01`、CASCADE で配下の matview（実体・索引宣言を含む）を消す
//! - 他テナントの同名 SCHEMA・matview に影響せず、応答にも現れない（`#`・内部 ID を出さない）
//! - 未対応の形（`ALTER MATERIALIZED VIEW .. SET SCHEMA`）は fail-closed で拒否する（存在する SCHEMA では `42601`）

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
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

fn session_for(tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    assert!(s.bind_resolution_tenant(tenant));
    s
}

fn ok_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{tenant}: {sql}: {} {e}", e.wire_code()))
}

fn err_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> (String, String) {
    match core.execute_sql_in_session(&ctx(tenant), s, sql) {
        Ok(o) => panic!("{tenant}: {sql} must fail, got {o:?}"),
        Err(e) => (e.wire_code().to_string(), e.to_string()),
    }
}

fn rows_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> Vec<String> {
    match ok_as(core, tenant, s, sql) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| format!("{:?}", row.cells))
            .collect(),
        other => panic!("{sql}: expected Query, got {other:?}"),
    }
}

const A: &str = "alice";

fn ok(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    ok_as(core, A, s, sql)
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    err_as(core, A, s, sql).0
}

fn rows(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<String> {
    rows_as(core, A, s, sql)
}

fn int_rows(ids: &[i64]) -> Vec<String> {
    ids.iter()
        .map(|i| format!("[SignedInteger({i})]"))
        .collect()
}

/// SCHEMA `s` に表 `s.t`（id 1・2）、public に表 `t`（id 9）を持つ世界。
fn world(label: &str) -> (EngineCore, CleanupGuard, SessionState) {
    let (core, g) = new_core(label);
    let mut s = session_for(A);
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO s.t (id, name) VALUES (1, 'a')");
    ok(&core, &mut s, "INSERT INTO s.t (id, name) VALUES (2, 'b')");
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO t (id, name) VALUES (9, 'p')");
    (core, g, s)
}

/// 修飾名での作成・参照・更新・削除（`IF NOT EXISTS`・`IF EXISTS`・`WITH NO DATA`・複数名 DROP を含む）。
#[test]
fn qualified_matview_lifecycle() {
    let (core, _g, mut s) = world("t27-mv-life");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv AS SELECT id, name FROM s.t",
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv ORDER BY id LIMIT 10"),
        int_rows(&[1, 2])
    );
    // 基表の更新は REFRESH までは反映されない。
    ok(&core, &mut s, "INSERT INTO s.t (id, name) VALUES (3, 'c')");
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv ORDER BY id LIMIT 10"),
        int_rows(&[1, 2])
    );
    ok(&core, &mut s, "REFRESH MATERIALIZED VIEW s.mv");
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv ORDER BY id LIMIT 10"),
        int_rows(&[1, 2, 3])
    );
    // 同名・IF NOT EXISTS。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.t"
        ),
        "42P07"
    );
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW IF NOT EXISTS s.mv AS SELECT id FROM s.t",
    );
    // 同名の表・ビューとも衝突する。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW s.t AS SELECT id FROM s.t"
        ),
        "42P07"
    );
    // WITH NO DATA は読み取りが 55000、REFRESH で実体化される。
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.nd AS SELECT id FROM s.t WITH NO DATA",
    );
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.nd LIMIT 5"), "55000");
    ok(&core, &mut s, "REFRESH MATERIALIZED VIEW s.nd");
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.nd ORDER BY id LIMIT 10"),
        int_rows(&[1, 2, 3])
    );
    // 連鎖: SCHEMA 配下の matview が SCHEMA 配下の matview を読む。
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv2 AS SELECT id FROM s.mv",
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv2 ORDER BY id LIMIT 10"),
        int_rows(&[1, 2, 3])
    );
    assert_eq!(code(&core, &mut s, "DROP MATERIALIZED VIEW s.mv"), "2BP01");
    ok(&core, &mut s, "DROP MATERIALIZED VIEW s.mv2, s.nd");
    ok(&core, &mut s, "DROP MATERIALIZED VIEW s.mv");
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.mv LIMIT 5"), "42P01");
    assert_eq!(code(&core, &mut s, "DROP MATERIALIZED VIEW s.mv"), "42P01");
    ok(&core, &mut s, "DROP MATERIALIZED VIEW IF EXISTS s.mv");
    assert_eq!(
        code(&core, &mut s, "REFRESH MATERIALIZED VIEW s.mv"),
        "42P01"
    );
    // 存在しない SCHEMA は 3F000。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW nope.m AS SELECT id FROM s.t"
        ),
        "3F000"
    );
    assert_eq!(
        code(&core, &mut s, "SELECT id FROM nope.m LIMIT 5"),
        "3F000"
    );
    // 再作成できる（旧実体が復活しない）。
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.t WHERE id = 1",
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv LIMIT 10"),
        int_rows(&[1])
    );
}

/// 表・ビュー・matview は relation の名前空間を共有する（SCHEMA 内でも衝突する）。
#[test]
fn matview_shares_the_relation_namespace_inside_a_schema() {
    let (core, _g, mut s) = world("t27-mv-ns");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.t",
    );
    assert_eq!(
        code(&core, &mut s, "CREATE VIEW s.mv AS SELECT id FROM s.t"),
        "42P07"
    );
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE s.mv (id BIGINT PRIMARY KEY)"),
        "42P07"
    );
    // DROP VIEW で matview は消せない（種別違い）。
    assert_eq!(code(&core, &mut s, "DROP VIEW s.mv"), "42809");
}

/// `search_path` 経由の未修飾名。`public.mv` と `s.mv` は同名で共存し、互いに影響しない。
#[test]
fn unqualified_matview_through_search_path_is_independent_of_public() {
    let (core, _g, mut s) = world("t27-mv-path");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW public.mv AS SELECT id FROM public.t",
    );
    ok(&core, &mut s, "SET search_path = s, public");
    // path 先頭の SCHEMA へ作る。本文の未修飾名は path で解決される（s.t が先）。
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM t",
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv ORDER BY id LIMIT 10"),
        int_rows(&[1, 2])
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM public.mv LIMIT 10"),
        int_rows(&[9])
    );
    // 未修飾の SELECT は s.mv（path 先頭）を読む。
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM mv ORDER BY id LIMIT 10"),
        int_rows(&[1, 2])
    );
    // REFRESH は s.mv だけを更新する。
    ok(&core, &mut s, "INSERT INTO s.t (id, name) VALUES (3, 'c')");
    ok(
        &core,
        &mut s,
        "INSERT INTO public.t (id, name) VALUES (10, 'q')",
    );
    ok(&core, &mut s, "REFRESH MATERIALIZED VIEW mv");
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv ORDER BY id LIMIT 10"),
        int_rows(&[1, 2, 3])
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM public.mv LIMIT 10"),
        int_rows(&[9])
    );
    // DROP も s.mv だけを消す。
    ok(&core, &mut s, "DROP MATERIALIZED VIEW mv");
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.mv LIMIT 5"), "42P01");
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM public.mv LIMIT 10"),
        int_rows(&[9])
    );
    // public が先頭なら未修飾の作成は public へ（s には作らない）。
    ok(&core, &mut s, "SET search_path = public, s");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW pm AS SELECT id FROM public.t",
    );
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.pm LIMIT 5"), "42P01");
    ok(&core, &mut s, "REFRESH MATERIALIZED VIEW public.mv");
    assert_eq!(
        rows(
            &core,
            &mut s,
            "SELECT id FROM public.mv ORDER BY id LIMIT 10"
        ),
        int_rows(&[9, 10])
    );
}

/// 拡張クエリ（Parse／Bind／Execute）でも CREATE・REFRESH・SELECT・DROP が通る。
#[test]
fn matview_statements_work_in_extended_queries() {
    let (core, _g, mut s) = world("t27-mv-ext");
    let c = ctx(A);
    ok(&core, &mut s, "SET search_path = s, public");
    for sql in [
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM t",
        "REFRESH MATERIALIZED VIEW mv",
        "CREATE MATERIALIZED VIEW s.q AS SELECT id FROM s.t",
        "REFRESH MATERIALIZED VIEW s.q",
    ] {
        let p = core
            .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
            .unwrap_or_else(|e| panic!("{sql}: parse {e}"));
        let _ = core.describe_prepared_in_session(&s, &p);
        let b = core
            .bind_prepared_in_session(&s, &p, &[])
            .unwrap_or_else(|e| panic!("{sql}: bind {e}"));
        core.execute_parsed_in_session(&c, &mut s, &b)
            .unwrap_or_else(|e| panic!("{sql}: execute {} {e}", e.wire_code()));
    }
    let p = core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT id FROM mv LIMIT 10", &[])
        .expect("parse select");
    let b = core.bind_prepared_in_session(&s, &p, &[]).expect("bind");
    match core
        .execute_parsed_in_session(&c, &mut s, &b)
        .expect("exec select")
    {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
    // Parse 後に path が変わると Bind は新しい path で検証し直す（public に mv は無い）。
    let p = core
        .parse_sql_prepared_with_declared_in_session(&s, "DROP MATERIALIZED VIEW mv", &[])
        .expect("parse drop");
    ok(&core, &mut s, "SET search_path = public");
    // public には mv が無いので、Bind 以降のいずれかで `42P01` になる（s.mv は黙って消えない）。
    let r = core
        .bind_prepared_in_session(&s, &p, &[])
        .and_then(|b| core.execute_parsed_in_session(&c, &mut s.clone(), &b));
    assert_eq!(r.expect_err("drop after path change").wire_code(), "42P01");
    ok(&core, &mut s, "SET search_path = s, public");
    for sql in ["DROP MATERIALIZED VIEW mv", "DROP MATERIALIZED VIEW s.q"] {
        let p = core
            .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
            .unwrap_or_else(|e| panic!("{sql}: parse {e}"));
        let b = core
            .bind_prepared_in_session(&s, &p, &[])
            .unwrap_or_else(|e| panic!("{sql}: bind {e}"));
        core.execute_parsed_in_session(&c, &mut s, &b)
            .unwrap_or_else(|e| panic!("{sql}: execute {} {e}", e.wire_code()));
    }
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.mv LIMIT 5"), "42P01");
}

/// `DROP SCHEMA`: RESTRICT は matview があれば `2BP01`。CASCADE は配下の matview・索引宣言・権限を消し、
/// 同名で作り直しても旧実体・旧宣言が復活しない。
#[test]
fn drop_schema_restrict_and_cascade_cover_matviews() {
    let (core, _g, mut s) = world("t27-mv-dropschema");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv AS SELECT id, name FROM s.t",
    );
    ok(&core, &mut s, "CREATE UNIQUE INDEX mvi ON s.mv (id)");
    assert_eq!(code(&core, &mut s, "DROP SCHEMA s"), "2BP01");
    assert_eq!(code(&core, &mut s, "DROP SCHEMA s RESTRICT"), "2BP01");
    // CASCADE は配下を全部消す。public の同名オブジェクトは残る。
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW public.mv AS SELECT id FROM public.t",
    );
    ok(&core, &mut s, "DROP SCHEMA s CASCADE");
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM public.mv LIMIT 10"),
        int_rows(&[9])
    );
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.mv LIMIT 5"), "3F000");
    // 同名の SCHEMA を作り直しても何も復活しない。
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TABLE s.t (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "INSERT INTO s.t (id) VALUES (7)");
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.mv LIMIT 5"), "42P01");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.t",
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv LIMIT 10"),
        int_rows(&[7])
    );
    // 索引名は掃除済み（旧宣言が残っていれば衝突する）。
    ok(&core, &mut s, "CREATE UNIQUE INDEX mvi ON s.mv (id)");
}

/// CASCADE は、別 SCHEMA の matview が消える SCHEMA の表を読む場合（外部依存）も一緒に消す。
#[test]
fn drop_schema_cascade_removes_external_matview_dependents() {
    let (core, _g, mut s) = world("t27-mv-ext-dep");
    ok(&core, &mut s, "CREATE SCHEMA u");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW u.mv AS SELECT id FROM s.t",
    );
    assert_eq!(code(&core, &mut s, "DROP SCHEMA s"), "2BP01");
    ok(&core, &mut s, "DROP SCHEMA s CASCADE");
    assert_eq!(code(&core, &mut s, "SELECT id FROM u.mv LIMIT 5"), "42P01");
    ok(&core, &mut s, "DROP SCHEMA u");
}

/// 依存: 表の DROP は RESTRICT で `2BP01`、CASCADE で matview も消える。RENAME・SET SCHEMA は `2BP01`。
#[test]
fn matview_dependencies_block_table_changes() {
    let (core, _g, mut s) = world("t27-mv-deps");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.t",
    );
    assert_eq!(code(&core, &mut s, "DROP TABLE s.t"), "2BP01");
    // 改名・SCHEMA 移動は拒否される。応答は public の表と matview の組と同じ（SCHEMA 配置で変わらない）。
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW pm AS SELECT id FROM public.t",
    );
    let public_rename = code(&core, &mut s, "ALTER TABLE public.t RENAME TO t3");
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE s.t RENAME TO t3"),
        public_rename
    );
    ok(&core, &mut s, "CREATE SCHEMA s2");
    // 依存する matview は SCHEMA 間の移動も `2BP01` で拒否する（既存の ALTER TABLE 契約）。
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE s.t SET SCHEMA s2"),
        "2BP01"
    );
    ok(&core, &mut s, "DROP TABLE s.t CASCADE");
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.mv LIMIT 5"), "42P01");
}

/// D7: public の matview が SCHEMA の表を読む形と、SCHEMA の matview が public を読む形は拒否される。
#[test]
fn matview_d7_rejects_cross_namespace_dependencies() {
    let (core, _g, mut s) = world("t27-mv-d7");
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW s.m AS SELECT id FROM t"
        ),
        "42501"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW pm AS SELECT id FROM s.t"
        ),
        "42501"
    );
    // 拒否された作成は何も残さない。
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.m AS SELECT id FROM s.t",
    );
}

/// 索引と `REFRESH ... CONCURRENTLY`。
#[test]
fn matview_index_and_concurrent_refresh_in_schema() {
    let (core, _g, mut s) = world("t27-mv-index");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv AS SELECT id, name FROM s.t",
    );
    ok(&core, &mut s, "CREATE UNIQUE INDEX mvi ON s.mv (id)");
    ok(&core, &mut s, "INSERT INTO s.t (id, name) VALUES (3, 'c')");
    ok(&core, &mut s, "REFRESH MATERIALIZED VIEW CONCURRENTLY s.mv");
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv ORDER BY id LIMIT 10"),
        int_rows(&[1, 2, 3])
    );
    ok(&core, &mut s, "DROP INDEX s.mvi");
}

/// `ALTER MATERIALIZED VIEW .. SET SCHEMA` は修飾・未修飾とも fail-closed で拒否する（対象外）。
// intentional: docs/design/schema-tenant-namespace.md
// 根拠の節: ALTER MATERIALIZED VIEW ... SET SCHEMA は意図的な拒否（#3287 の追記の節）
#[test]
fn alter_matview_set_schema_stays_unsupported() {
    let (core, _g, mut s) = world("t27-mv-alter");
    ok(&core, &mut s, "CREATE SCHEMA s2");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.t",
    );
    for sql in [
        "ALTER MATERIALIZED VIEW s.mv SET SCHEMA s2",
        "ALTER MATERIALIZED VIEW s.mv SET SCHEMA public",
    ] {
        let (c, m) = err_as(&core, A, &mut s, sql);
        assert_eq!(c, "42601", "{sql}: {m}");
        assert!(!m.contains('#'), "{sql}: {m}");
    }
    ok(&core, &mut s, "SET search_path = s, public");
    let (c, m) = err_as(&core, A, &mut s, "ALTER MATERIALIZED VIEW mv SET SCHEMA s2");
    assert_eq!(c, "42601", "{m}");
    assert!(!m.contains('#'), "{m}");
    // 失敗しても matview は元の場所に残る。
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s.mv ORDER BY id LIMIT 10"),
        int_rows(&[1, 2])
    );
    assert_eq!(code(&core, &mut s, "SELECT id FROM s2.mv LIMIT 5"), "42P01");
}

/// 他テナントの同名 SCHEMA・同名 matview は互いに影響しない。
#[test]
fn same_named_matviews_in_two_tenants_are_independent() {
    let (core, _g) = new_core("t27-mv-tenants");
    let mut sa = session_for("alice");
    let mut sb = session_for("bob");
    for (t, s, id) in [("alice", &mut sa, 1), ("bob", &mut sb, 2)] {
        ok_as(&core, t, s, "CREATE SCHEMA s");
        ok_as(&core, t, s, "CREATE TABLE s.t (id BIGINT PRIMARY KEY)");
        ok_as(&core, t, s, &format!("INSERT INTO s.t (id) VALUES ({id})"));
        ok_as(
            &core,
            t,
            s,
            "CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.t",
        );
    }
    ok_as(&core, "alice", &mut sa, "INSERT INTO s.t (id) VALUES (11)");
    ok_as(&core, "bob", &mut sb, "INSERT INTO s.t (id) VALUES (22)");
    ok_as(&core, "alice", &mut sa, "REFRESH MATERIALIZED VIEW s.mv");
    assert_eq!(
        rows_as(
            &core,
            "alice",
            &mut sa,
            "SELECT id FROM s.mv ORDER BY id LIMIT 10"
        ),
        int_rows(&[1, 11])
    );
    // bob の実体は alice の REFRESH で変わらない。
    assert_eq!(
        rows_as(
            &core,
            "bob",
            &mut sb,
            "SELECT id FROM s.mv ORDER BY id LIMIT 10"
        ),
        int_rows(&[2])
    );
    ok_as(&core, "alice", &mut sa, "DROP MATERIALIZED VIEW s.mv");
    assert_eq!(
        rows_as(&core, "bob", &mut sb, "SELECT id FROM s.mv LIMIT 10"),
        int_rows(&[2])
    );
    ok_as(&core, "bob", &mut sb, "REFRESH MATERIALIZED VIEW s.mv");
    assert_eq!(
        rows_as(
            &core,
            "bob",
            &mut sb,
            "SELECT id FROM s.mv ORDER BY id LIMIT 10"
        ),
        int_rows(&[2, 22])
    );
}

fn bob_shape(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx("bob"), s, sql) {
        Ok(o) => format!("OK {o:?}"),
        Err(e) => format!("ERR {} {e}", e.wire_code()),
    }
}

/// 他テナントにだけ SCHEMA・matview がある世界と、どこにも無い世界で、閲覧側の応答（SQLSTATE・文言）が
/// 完全一致する。応答に `#` が出ない。
#[test]
fn foreign_schema_matview_responses_match_a_world_without_it() {
    let (with, _g1) = new_core("t27-mv-twin-a");
    let (without, _g2) = new_core("t27-mv-twin-b");
    let mut owner = session_for("alice");
    ok_as(&with, "alice", &mut owner, "CREATE SCHEMA zz_secret_s");
    ok_as(
        &with,
        "alice",
        &mut owner,
        "CREATE TABLE zz_secret_s.t (id BIGINT PRIMARY KEY)",
    );
    ok_as(
        &with,
        "alice",
        &mut owner,
        "CREATE MATERIALIZED VIEW zz_secret_s.mv AS SELECT id FROM zz_secret_s.t",
    );
    let attempts = [
        "SELECT id FROM zz_secret_s.mv LIMIT 5",
        "REFRESH MATERIALIZED VIEW zz_secret_s.mv",
        "REFRESH MATERIALIZED VIEW CONCURRENTLY zz_secret_s.mv",
        "DROP MATERIALIZED VIEW zz_secret_s.mv",
        "DROP MATERIALIZED VIEW IF EXISTS zz_secret_s.mv",
        "CREATE MATERIALIZED VIEW zz_secret_s.mv AS SELECT id FROM zz_secret_s.t",
        "CREATE MATERIALIZED VIEW IF NOT EXISTS zz_secret_s.mv AS SELECT id FROM t",
        "ALTER MATERIALIZED VIEW zz_secret_s.mv OWNER TO bob",
        "ALTER MATERIALIZED VIEW zz_secret_s.mv SET SCHEMA public",
        "CREATE UNIQUE INDEX i ON zz_secret_s.mv (id)",
    ];
    for sql in attempts {
        let mut sw = session_for("bob");
        let mut sn = session_for("bob");
        let a = bob_shape(&with, &mut sw, sql);
        let b = bob_shape(&without, &mut sn, sql);
        assert_eq!(a, b, "{sql}");
        assert!(!a.contains('#'), "{sql}: {a}");
    }
    // 未修飾名（path に他テナントの SCHEMA 名を置く）も同じ。
    for sql in [
        "SELECT id FROM mv LIMIT 5",
        "REFRESH MATERIALIZED VIEW mv",
        "DROP MATERIALIZED VIEW mv",
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM t",
    ] {
        let mut sw = session_for("bob");
        let mut sn = session_for("bob");
        let set = "SET search_path = zz_secret_s, public";
        let a0 = bob_shape(&with, &mut sw, set);
        let b0 = bob_shape(&without, &mut sn, set);
        assert_eq!(a0, b0, "{set}");
        let a = bob_shape(&with, &mut sw, sql);
        let b = bob_shape(&without, &mut sn, sql);
        assert_eq!(a, b, "{sql}");
        assert!(!a.contains('#'), "{sql}: {a}");
    }
    // 持ち主の定義・実体は bob の試行で変わらない。
    assert_eq!(
        rows_as(
            &with,
            "alice",
            &mut owner,
            "SELECT id FROM zz_secret_s.mv LIMIT 5"
        )
        .len(),
        0
    );
}

/// 暗黙 RLS: 可視性の違う文脈ごとに実体が分かれる。SCHEMA 配下でも `Public` だけの文脈は
/// `Private` 込みの実体を読まない。
#[test]
fn matview_entities_stay_split_by_visibility_in_a_schema() {
    let (core, _g) = new_core("t27-mv-rls");
    let mut s = session_for(A);
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TABLE s.t (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "INSERT INTO s.t (id) VALUES (1)");
    let public_only = PolicyContext::new(A).expect("ctx");
    let core = core.with_materialization_contexts(vec![ctx(A), public_only.clone()]);
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.t",
    );
    // 作成者（Public + Private）の実体は読める。
    assert_eq!(rows(&core, &mut s, "SELECT id FROM s.mv LIMIT 10").len(), 1);
    // Public だけの文脈は自分の文脈で評価された実体だけを読む（Private 込みの実体は読めない）。
    let mut sp = session_for(A);
    match core.execute_sql_in_session(&public_only, &mut sp, "SELECT id FROM s.mv LIMIT 10") {
        Ok(SqlOutcome::Query(r)) => assert!(r.rows.len() <= 1),
        Err(e) => assert_eq!(e.wire_code(), "55000"),
        other => panic!("public-only read: {other:?}"),
    }
}

/// D-c: 実体化の対象文脈に他テナントの文脈を注入しても、SCHEMA 配下の matview の実体は作成者の
/// テナントの文脈にしか書かれない（`public` の matview は従来どおり全文脈）。
#[test]
fn schema_matview_entities_are_written_only_for_the_creator_tenant() {
    use redb::{ReadableDatabase, ReadableTable, TableDefinition};
    let path = unique_db_path("t27-mv-dc");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
            .with_materialization_contexts(vec![ctx("alice"), ctx("bob")]);
        let mut s = session_for("alice");
        ok_as(&core, "alice", &mut s, "CREATE SCHEMA s");
        ok_as(
            &core,
            "alice",
            &mut s,
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY)",
        );
        ok_as(
            &core,
            "alice",
            &mut s,
            "CREATE TABLE t (id BIGINT PRIMARY KEY)",
        );
        ok_as(
            &core,
            "alice",
            &mut s,
            "CREATE MATERIALIZED VIEW s.mv AS SELECT id FROM s.t",
        );
        ok_as(
            &core,
            "alice",
            &mut s,
            "CREATE MATERIALIZED VIEW pmv AS SELECT id FROM t",
        );
        ok_as(&core, "alice", &mut s, "REFRESH MATERIALIZED VIEW s.mv");
    }
    let db = redb::Database::open(&path).expect("reopen");
    let rt = db.begin_read().expect("read");
    let meta: TableDefinition<(&str, &[u8]), &[u8]> = TableDefinition::new("matview_entities");
    let table = rt.open_table(meta).expect("meta table");
    let (mut schema_entities, mut public_entities) = (0usize, 0usize);
    for e in table.iter().expect("iter") {
        let (k, _) = e.expect("entry");
        let (name, _) = k.value();
        if name.starts_with('#') {
            schema_entities += 1;
        } else if name == "pmv" {
            public_entities += 1;
        }
    }
    assert_eq!(
        schema_entities, 1,
        "schema-scoped matview: creator tenant only"
    );
    assert_eq!(
        public_entities, 2,
        "public matview keeps every injected context"
    );
}
