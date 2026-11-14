//! セッション変数 `search_path`（TABLE-27・Issue #1715）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-27 (3)、`docs/spec/04-behavior/error-format.md`
//! ERR-6。設計記録は `docs/design/schema-tenant-namespace.md`（D5・D6・D11。実装記録は §8）。
//!
//! `table27_qualified_names.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - 既定値 `"$user", public`・`SET`／`SHOW`／`RESET`・不正値の `22023`・`SET LOCAL` の `0A000`
//! - 存在しない SCHEMA は無視される（`nosuch, public` は既定と同じに解決する）
//! - `public` が見えない path では一時表以外の未修飾名が `42P01`（`public.` 修飾・CTE・定数式は通る）
//! - 作成先の判定（`3F000`／`0A000`／`public.` 明示修飾）と DDL 権限の判定順
//! - `"$user"` の展開（handshake 相当の `set_session_user`）
//! - ROLLBACK での取り消し、拡張クエリ（Parse 後の `SET` で陳腐化した文を実行しない）

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "alice";

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

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn ok(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(TENANT), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()))
}

fn err(core: &EngineCore, s: &mut SessionState, sql: &str) -> (String, String) {
    match core.execute_sql_in_session(&ctx(TENANT), s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => (e.wire_code().to_string(), e.to_string()),
    }
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    err(core, s, sql).0
}

fn show(core: &EngineCore, s: &mut SessionState) -> String {
    match ok(core, s, "SHOW search_path") {
        SqlOutcome::Show(r) => match &r.rows[0].cells[0] {
            Cell::Text(t) => t.clone(),
            other => panic!("expected text cell, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

/// `public` に表 `t`（1 行）を作った状態のコアとセッション。
fn with_table(label: &str) -> (EngineCore, CleanupGuard, SessionState) {
    let (core, g) = new_core(label);
    let mut s = ddl_session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO t (id, name) VALUES (1, 'a')");
    (core, g, s)
}

#[test]
fn default_value_and_set_show_reset_roundtrip() {
    let (core, _g) = new_core("sp-roundtrip");
    let mut s = ddl_session();
    assert_eq!(show(&core, &mut s), "\"$user\", public");
    ok(&core, &mut s, "SET search_path TO a, public");
    assert_eq!(show(&core, &mut s), "a, public");
    ok(&core, &mut s, "SET search_path = DEFAULT");
    assert_eq!(show(&core, &mut s), "\"$user\", public");
    ok(&core, &mut s, "SET SESSION search_path = b");
    assert_eq!(show(&core, &mut s), "b");
    assert_eq!(
        ok(&core, &mut s, "RESET search_path"),
        SqlOutcome::SetSearchPath { reset: true }
    );
    assert_eq!(show(&core, &mut s), "\"$user\", public");
    // 大小無視の正規化・引用・文字列リテラル。
    ok(
        &core,
        &mut s,
        "SET search_path TO PUBLIC, \"MySchema\", 'lit', \"$user\"",
    );
    assert_eq!(show(&core, &mut s), "public, \"MySchema\", lit, \"$user\"");
    ok(&core, &mut s, "SET search_path TO '$user'");
    assert_eq!(show(&core, &mut s), "\"$user\"");
}

#[test]
fn set_search_path_values_are_not_scanned_as_relations() {
    let (core, _g) = new_core("sp-set-values");
    let mut s = ddl_session();
    ok(&core, &mut s, "SET search_path = nosuch");
    // 設定値の `truncate` を TRUNCATE 文と誤解釈せず、`public` を表参照として拒否しない。
    ok(&core, &mut s, "SET search_path TO truncate, public");
    assert_eq!(show(&core, &mut s), "truncate, public");
}

#[test]
fn invalid_values_fail_closed_without_changing_the_value() {
    let (core, _g) = new_core("sp-invalid");
    let mut s = ddl_session();
    ok(&core, &mut s, "SET search_path TO keep");
    assert_eq!(code(&core, &mut s, "SET LOCAL search_path TO x"), "0A000");
    assert_eq!(code(&core, &mut s, "SET search_path TO ''"), "22023");
    assert_eq!(code(&core, &mut s, "SET search_path TO 'a,b'"), "22023");
    let long = "x".repeat(64);
    assert_eq!(
        code(&core, &mut s, &format!("SET search_path TO {long}")),
        "22023"
    );
    let many = (0..65)
        .map(|i| format!("s{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        code(&core, &mut s, &format!("SET search_path TO {many}")),
        "22023"
    );
    let max = (0..64)
        .map(|i| format!("s{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    ok(&core, &mut s, &format!("SET search_path TO {max}"));
    ok(&core, &mut s, "SET search_path TO keep");
    assert_eq!(
        code(&core, &mut s, "SET search_path TO public, pg_temp"),
        "0A000"
    );
    ok(&core, &mut s, "SET search_path TO pg_temp, public");
    ok(&core, &mut s, "SET search_path TO keep");
    // 構文エラーと、他の変数・RESET の従来契約。
    assert_eq!(code(&core, &mut s, "SET search_path TO"), "42601");
    assert_eq!(code(&core, &mut s, "SET search_path TO a,"), "42601");
    assert_eq!(code(&core, &mut s, "SET search_path TO 1"), "42601");
    assert_eq!(code(&core, &mut s, "RESET ALL"), "42601");
    assert_eq!(code(&core, &mut s, "SET other_var = 1"), "42601");
    assert_eq!(show(&core, &mut s), "keep");
}

#[test]
fn quoted_dollar_user_is_only_accepted_as_a_search_path_element() {
    let (core, _g, mut s) = with_table("sp-dollar-user");
    assert_eq!(code(&core, &mut s, "SELECT \"$user\" FROM t"), "42601");
    assert_eq!(code(&core, &mut s, "SELECT id FROM \"$user\""), "42601");
    assert_eq!(code(&core, &mut s, "SET search_path TO $user"), "42601");
}

#[test]
fn session_less_entry_rejects_set_and_show() {
    let (core, _g) = new_core("sp-sessionless");
    for sql in [
        "SET search_path = a",
        "SHOW search_path",
        "RESET search_path",
    ] {
        let e = core.execute_sql(&ctx(TENANT), sql).expect_err(sql);
        assert_eq!(e.wire_code(), "42601", "{sql}");
    }
}

#[test]
fn nonexistent_schemas_are_ignored_when_public_is_listed() {
    let (core, _g, mut s) = with_table("sp-ignore-missing");
    ok(
        &core,
        &mut s,
        "SET search_path = nosuch, other_tenant_schema, public",
    );
    ok(&core, &mut s, "SELECT id FROM t LIMIT 5");
    ok(&core, &mut s, "INSERT INTO t (id, name) VALUES (2, 'b')");
    ok(&core, &mut s, "UPDATE t SET name = 'c' WHERE id = 2");
    ok(&core, &mut s, "DELETE FROM t WHERE id = 2");
    ok(&core, &mut s, "CREATE TABLE u (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "SELECT id FROM u LIMIT 5");
}

#[test]
fn unqualified_names_are_rejected_when_public_is_not_on_the_path() {
    let (core, _g, mut s) = with_table("sp-hidden");
    ok(&core, &mut s, "SET search_path = nosuch");
    let stmts = [
        "SELECT id FROM t LIMIT 5",
        "SELECT a.id FROM t a JOIN t b ON a.id = b.id LIMIT 5",
        "SELECT id FROM t WHERE id IN (SELECT id FROM t LIMIT 5) LIMIT 5",
        "SELECT COUNT(*) FROM t",
        "INSERT INTO t (id, name) VALUES (9, 'z')",
        "UPDATE t SET name = 'z' WHERE id = 1",
        "DELETE FROM t WHERE id = 1",
        "TRUNCATE t",
        "DROP TABLE t",
        "DROP TABLE IF EXISTS t",
        "ALTER TABLE t ADD COLUMN extra TEXT",
        "CREATE INDEX t_name ON t (name)",
        "CREATE TABLE c (id BIGINT PRIMARY KEY, pid BIGINT REFERENCES t (id))",
        "CREATE VIEW v AS SELECT id FROM t",
        "WITH c AS (SELECT id, name FROM t WHERE name = 'a') SELECT id FROM c WHERE name = 'a' LIMIT 10",
        "EXPLAIN SELECT id FROM t LIMIT 5",
        "DROP VIEW v",
        "DROP VIEW IF EXISTS v",
        "DROP SEQUENCE sq",
        "DROP INDEX i",
        "DROP TYPE ty",
        "REFRESH MATERIALIZED VIEW m",
    ];
    for sql in stmts {
        let (c, msg) = err(&core, &mut s, sql);
        assert_eq!(c, "42P01", "{sql}: {msg}");
        // 文言は利用者が書いた名前だけで、内部情報を含まない。
        assert!(!msg.contains('#'), "{sql}: {msg}");
    }
    // 修飾名・CTE・定数式・SET／SHOW は影響を受けない。
    ok(&core, &mut s, "SELECT id FROM public.t LIMIT 5");
    ok(
        &core,
        &mut s,
        "INSERT INTO public.t (id, name) VALUES (9, 'z')",
    );
    ok(&core, &mut s, "SELECT 1");
    ok(&core, &mut s, "SHOW search_path");
    ok(
        &core,
        &mut s,
        "WITH c AS (SELECT id, name FROM public.t WHERE name = 'a') SELECT id FROM c WHERE name = 'a' LIMIT 10",
    );
    // 一時表は path に関わらず先頭で解決される。
    ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY)",
    );
    ok(&core, &mut s, "INSERT INTO tmp (id) VALUES (1)");
    ok(&core, &mut s, "SELECT id FROM tmp LIMIT 5");
    ok(&core, &mut s, "DROP TABLE tmp");
    // path を戻せば解決できる。
    ok(&core, &mut s, "RESET search_path");
    ok(&core, &mut s, "SELECT id FROM t LIMIT 5");
}

#[test]
fn syntax_errors_win_and_ddl_permission_is_checked_first() {
    let (core, _g, mut s) = with_table("sp-order");
    ok(&core, &mut s, "SET search_path = nosuch");
    assert_eq!(code(&core, &mut s, "SELECT FROM FROM t"), "42601");
    // DDL 権限の無いセッションは存在判定より前に `42501`。
    let mut plain = SessionState::default();
    ok(&core, &mut plain, "SET search_path = nosuch");
    assert_eq!(code(&core, &mut plain, "DROP TABLE t"), "42501");
    assert_eq!(
        code(&core, &mut plain, "CREATE TABLE u (id BIGINT PRIMARY KEY)"),
        "42501"
    );
}

#[test]
fn copy_is_gated_by_search_path() {
    let (core, _g, mut s) = with_table("sp-copy");
    ok(&core, &mut s, "SET search_path = nosuch");
    for sql in [
        "COPY (SELECT id FROM t LIMIT 5) TO STDOUT",
        "COPY t (id, name) FROM STDIN",
    ] {
        match core.begin_copy(&ctx(TENANT), &s, sql) {
            Err(e) => assert_eq!(e.wire_code(), "42P01", "{sql}"),
            Ok(_) => panic!("{sql} must fail"),
        }
    }
    assert!(core
        .begin_copy(
            &ctx(TENANT),
            &s,
            "COPY (SELECT id FROM public.t LIMIT 5) TO STDOUT"
        )
        .is_ok());
}

// intentional: docs/design/schema-tenant-namespace.md（ENUM 型・MATERIALIZED VIEW・組み込み SCHEMA への作成は対象外）
#[test]
fn creation_target_follows_the_path() {
    let (core, _g, mut s) = with_table("sp-create");
    ok(&core, &mut s, "SET search_path = nosuch");
    let (c, msg) = err(&core, &mut s, "CREATE TABLE u (id BIGINT PRIMARY KEY)");
    assert_eq!(c, "3F000");
    assert_eq!(msg, "no schema has been selected to create in");
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE IF NOT EXISTS u (id BIGINT PRIMARY KEY)"
        ),
        "3F000"
    );
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE sq"), "3F000");
    assert_eq!(code(&core, &mut s, "CREATE TYPE ty AS ENUM ('a')"), "3F000");
    // `public.` で明示修飾した作成は path に依らない。
    ok(
        &core,
        &mut s,
        "CREATE TABLE public.u (id BIGINT PRIMARY KEY)",
    );
    ok(&core, &mut s, "SELECT id FROM public.u LIMIT 5");
    // 一時表の作成は path に依らない。
    ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY)",
    );

    // 自テナントの既存 SCHEMA が先頭なら、その SCHEMA へ作る（Issue #3196）。
    ok(&core, &mut s, "RESET search_path");
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "SET search_path = s, public");
    ok(&core, &mut s, "CREATE TABLE w (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "CREATE SEQUENCE sq2");
    // 作った表・SEQUENCE は s の中にあり、public には無い。
    ok(&core, &mut s, "SELECT id FROM s.w LIMIT 5");
    assert_eq!(
        code(&core, &mut s, "SELECT id FROM public.w LIMIT 5"),
        "42P01"
    );
    assert_eq!(code(&core, &mut s, "DROP SEQUENCE public.sq2"), "42P01");
    ok(&core, &mut s, "DROP SEQUENCE s.sq2");
    // 読み取りは public の表を引ける（空でない s に無い名前は後ろの public へ落ちる）。
    ok(&core, &mut s, "SELECT id FROM t LIMIT 5");
    // SCHEMA 配置が未対応の作成（ENUM 型・MATERIALIZED VIEW）は public へ黙って作らず `0A000`。
    assert_eq!(
        code(&core, &mut s, "CREATE TYPE ty2 AS ENUM ('a')"),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW mv AS SELECT id FROM t"
        ),
        "0A000"
    );
    // 組み込み SCHEMA・pg_temp への作成は拒否する。
    ok(&core, &mut s, "SET search_path = pg_catalog, public");
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE w (id BIGINT PRIMARY KEY)"),
        "0A000"
    );
    ok(&core, &mut s, "SET search_path = pg_temp, public");
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE w (id BIGINT PRIMARY KEY)"),
        "0A000"
    );
}

#[test]
fn other_tenants_schemas_are_indistinguishable_from_missing_ones() {
    let (core, _g) = new_core("sp-other-tenant");
    let mut s = ddl_session();
    // 他テナントが SCHEMA `theirs` を作る。
    core.execute_sql_in_session(&ctx("bob"), &mut ddl_session(), "CREATE SCHEMA theirs")
        .expect("bob creates schema");
    ok(&core, &mut s, "SET search_path = theirs, public");
    ok(&core, &mut s, "CREATE TABLE u (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "SELECT id FROM u LIMIT 5");
}

#[test]
fn dollar_user_expands_from_the_session_user_only() {
    let (core, _g) = new_core("sp-user");
    let mut s = ddl_session();
    // ユーザー未設定（Rust API）は `$user` を飛ばす。
    ok(&core, &mut s, "CREATE SCHEMA alice");
    ok(&core, &mut s, "CREATE TABLE u0 (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "DROP TABLE u0");
    // username と同名の SCHEMA が既定の path で先に見つかり、そこへ作られる（Issue #3196）。
    s.set_session_user("alice");
    ok(&core, &mut s, "CREATE TABLE u1 (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "SELECT id FROM alice.u1 LIMIT 5");
    assert_eq!(
        code(&core, &mut s, "SELECT id FROM public.u1 LIMIT 5"),
        "42P01"
    );
    // 同名の SCHEMA が無いユーザーは `public` へ作る。
    let mut other = ddl_session();
    other.set_session_user("carol");
    ok(&core, &mut other, "CREATE TABLE u2 (id BIGINT PRIMARY KEY)");
    // `public` を含まない path でも、username が `public` なら public が見える。
    let mut pubuser = ddl_session();
    pubuser.set_session_user("Public");
    ok(&core, &mut pubuser, "SET search_path = \"$user\"");
    ok(&core, &mut pubuser, "SELECT id FROM u2 LIMIT 5");
    let mut nonpub = ddl_session();
    nonpub.set_session_user("carol");
    ok(&core, &mut nonpub, "SET search_path = \"$user\"");
    assert_eq!(
        code(&core, &mut nonpub, "SELECT id FROM u2 LIMIT 5"),
        "42P01"
    );
}

#[test]
fn rollback_restores_the_path_and_commit_keeps_it() {
    let (core, _g, mut s) = with_table("sp-txn");
    let c = ctx(TENANT);
    let mut txn = core.new_session_transaction();
    for sql in ["BEGIN", "SET search_path = nosuch"] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .expect(sql);
    }
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT id FROM t LIMIT 5")
        .expect_err("hidden public inside txn");
    assert_eq!(e.wire_code(), "42P01");
    // 構文・検証エラーで Failed へ遷移するため ROLLBACK で戻す。
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(show(&core, &mut s), "\"$user\", public");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT id FROM t LIMIT 5")
        .expect("default path resolves again");

    for sql in ["BEGIN", "SET search_path = x, public", "COMMIT"] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .expect(sql);
    }
    assert_eq!(show(&core, &mut s), "x, public");
}

#[test]
fn extended_query_revalidates_statements_after_the_path_changes() {
    let (core, _g, mut s) = with_table("sp-extended");
    let c = ctx(TENANT);
    // 既定の path で Parse した文。
    let plain = core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT id FROM t LIMIT 5", &[])
        .expect("parse");
    let with_param = core
        .parse_sql_prepared_with_declared_in_session(
            &s,
            "SELECT id FROM t WHERE id = $1 LIMIT 5",
            &[],
        )
        .expect("parse with param");
    let bound = core
        .bind_prepared_in_session(&s, &plain, &[])
        .expect("bind under default path");
    core.execute_parsed_in_session(&c, &mut s, &bound)
        .expect("execute under default path");

    // Parse 後に path が変わると、Bind は新しい path で検証し直す。
    ok(&core, &mut s, "SET search_path = nosuch");
    for p in [&plain, &with_param] {
        let values: &[Option<Vec<u8>>] = if p.param_count() == 0 {
            &[]
        } else {
            &[Some(b"1".to_vec())]
        };
        let e = core
            .bind_prepared_in_session(&s, p, values)
            .expect_err("bind after SET");
        assert_eq!(e.wire_code(), "42P01");
    }
    // Describe も同じ。
    let e = core
        .describe_prepared_in_session(&s, &plain)
        .expect_err("describe after SET");
    assert_eq!(e.wire_code(), "42P01");
    // path が隠れている状態の Parse は参照を拒否する（構文エラーが優先）。
    let e = core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT id FROM t LIMIT 5", &[])
        .expect_err("parse while hidden");
    assert_eq!(e.wire_code(), "42P01");
    let e = core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT FROM FROM t", &[])
        .expect_err("syntax first");
    assert_eq!(e.wire_code(), "42601");
    // 修飾名の文は隠れた path でも Parse／Bind できる。
    let qualified = core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT id FROM public.t LIMIT 5", &[])
        .expect("qualified parse");
    let qbound = core
        .bind_prepared_in_session(&s, &qualified, &[])
        .expect("qualified bind");
    core.execute_parsed_in_session(&c, &mut s, &qbound)
        .expect("qualified execute");

    // Bind 後・Execute 前に path が変わった文は陳腐化として拒否される。
    ok(&core, &mut s, "SET search_path = nosuch2");
    let e = core
        .execute_parsed_in_session(&c, &mut s, &qbound)
        .expect_err("stale after SET");
    assert_eq!(e.wire_code(), "0A000");
    // SET／SHOW は陳腐化の検査を受けない。
    let set = core
        .parse_sql_prepared_with_declared_in_session(&s, "SET search_path = DEFAULT", &[])
        .expect("parse set");
    let set_bound = core
        .bind_prepared_in_session(&s, &set, &[])
        .expect("bind set");
    ok(&core, &mut s, "SET search_path = nosuch3");
    core.execute_parsed_in_session(&c, &mut s, &set_bound)
        .expect("exempt statement still runs");
    assert_eq!(show(&core, &mut s), "\"$user\", public");
}

#[test]
fn unvalidated_parsed_statements_are_rejected_while_public_is_hidden() {
    let (core, _g, mut s) = with_table("sp-rust-api");
    let c = ctx(TENANT);
    let parsed = core
        .parse_sql("SELECT id FROM t LIMIT 5")
        .expect("parse_sql");
    core.execute_parsed_in_session(&c, &mut s, &parsed)
        .expect("default path executes as before");
    ok(&core, &mut s, "SET search_path = nosuch");
    let e = core
        .execute_parsed_in_session(&c, &mut s, &parsed)
        .expect_err("hidden path");
    assert_eq!(e.wire_code(), "0A000");
    // 免除文は実行できる。
    let reset = core.parse_sql("RESET search_path").expect("parse reset");
    core.execute_parsed_in_session(&c, &mut s, &reset)
        .expect("reset runs");
    core.execute_parsed_in_session(&c, &mut s, &parsed)
        .expect("visible again");
}

#[test]
fn search_path_never_widens_the_tenant_boundary() {
    let (core, _g) = new_core("sp-boundary");
    let mut a = ddl_session();
    ok(
        &core,
        &mut a,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT)",
    );
    ok(
        &core,
        &mut a,
        "INSERT INTO docs (id, body) VALUES (1, 'mine')",
    );
    let mut b = ddl_session();
    let cb = ctx("bob");
    core.execute_sql_in_session(&cb, &mut b, "SET search_path = alice, public")
        .expect("bob sets path to alice");
    let out = core
        .execute_sql_in_session(&cb, &mut b, "SELECT id FROM docs LIMIT 5")
        .expect("bob selects");
    match out {
        SqlOutcome::Query(r) => assert!(r.rows.is_empty(), "bob must not see alice's rows"),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn gate_respects_cte_scope_syntax_named_tables_and_temp_indexes() {
    let (core, _g, mut s) = with_table("sp-gate-review");
    ok(
        &core,
        &mut s,
        "CREATE TABLE \"temp\" (id BIGINT PRIMARY KEY)",
    );
    ok(&core, &mut s, "SET search_path = nosuch");
    // 非再帰 CTE の本文中の自己参照は同名の永続表を読むためゲートの対象。
    for sql in [
        "WITH t AS (SELECT id FROM t) SELECT id FROM t LIMIT 5",
        "SELECT id FROM temp LIMIT 5",
    ] {
        assert_eq!(code(&core, &mut s, sql), "42P01", "{sql}");
    }
    // 一時表の索引に対する DROP INDEX は一時側で解決される。
    ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY, v INT)",
    );
    ok(&core, &mut s, "CREATE INDEX tmp_v ON tmp (v)");
    ok(&core, &mut s, "DROP INDEX tmp_v");
    assert_eq!(code(&core, &mut s, "DROP INDEX tmp_v"), "42P01");
}

/// 引用付き `NULL` 名の表も参照ゲートの対象（`DROP TABLE "null"` が `42P01`。永続表は残る）。
#[test]
fn quoted_null_named_table_is_gated() {
    let (core, _g) = new_core("sp-quoted-null");
    let mut s = ddl_session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE \"null\" (id BIGINT PRIMARY KEY)",
    );
    ok(&core, &mut s, "SET search_path = nosuch");
    assert_eq!(code(&core, &mut s, "DROP TABLE \"null\""), "42P01");
    ok(&core, &mut s, "RESET search_path");
    ok(&core, &mut s, "DROP TABLE \"null\"");
}

/// 構文語と同名の別名（`AS truncate`）が表参照リストを開かず、`public.` 修飾の表だけの文は通る。
#[test]
fn syntax_word_alias_does_not_trigger_the_gate() {
    let (core, _g, mut s) = with_table("sp-alias");
    ok(&core, &mut s, "SET search_path = nosuch");
    ok(
        &core,
        &mut s,
        "SELECT 1 AS truncate, id FROM public.t LIMIT 5",
    );
}

/// 一時表と同名の search_path 要素でも `SET search_path` は一時表参照扱い（`0A000`）にならない。
#[test]
fn set_search_path_ignores_same_named_temp_table() {
    let (core, _g) = new_core("sp-temp-name");
    let mut s = ddl_session();
    ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE tmpx (id BIGINT PRIMARY KEY)",
    );
    ok(&core, &mut s, "SET search_path = tmpx, public");
    assert_eq!(show(&core, &mut s), "tmpx, public");
}

/// 表 `name` の行数（`SELECT id FROM <name> LIMIT 100` の結果行数）。`search_path` が `public` を
/// 含む状態のセッションで呼ぶ。
fn row_count(core: &EngineCore, s: &mut SessionState, name: &str) -> usize {
    match ok(core, s, &format!("SELECT id FROM {name} LIMIT 100")) {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 表 `a`・`b`（各 1 行）と、構文語と同名の表 `"in"`（1 行）を持つコアとセッション。
fn with_two_tables(label: &str) -> (EngineCore, CleanupGuard, SessionState) {
    let (core, g) = new_core(label);
    let mut s = ddl_session();
    for t in ["a", "b", "\"in\""] {
        ok(
            &core,
            &mut s,
            &format!("CREATE TABLE {t} (id BIGINT PRIMARY KEY, name TEXT)"),
        );
        ok(
            &core,
            &mut s,
            &format!("INSERT INTO {t} (id, name) VALUES (1, 'x')"),
        );
    }
    (core, g, s)
}

/// `TRUNCATE` の表リストでカンマに続く `ONLY` の対象表も参照ゲートに掛かる（codex P1。
/// 未修飾の永続表 `b` を `42P01` にせず削除してはならない）。拒否された文は表の内容を変えない。
#[test]
fn truncate_list_only_target_is_gated() {
    let (core, _g, mut s) = with_two_tables("sp-trunc-only");
    ok(&core, &mut s, "SET search_path = nosuch");
    for sql in [
        "TRUNCATE public.a, ONLY b",
        "TRUNCATE a, ONLY b",
        "TRUNCATE public.a, ONLY public.b, ONLY c",
    ] {
        assert_eq!(code(&core, &mut s, sql), "42P01", "{sql}");
    }
    ok(&core, &mut s, "RESET search_path");
    for t in ["a", "b", "\"in\""] {
        assert_eq!(row_count(&core, &mut s, t), 1, "{t} must be untouched");
    }
}

/// `TRUNCATE public.a, ONLY public.b` は修飾の書き換え漏れなく両方の表へ解決される。
#[test]
fn truncate_list_only_qualified_names_are_rewritten() {
    let (core, _g, mut s) = with_two_tables("sp-trunc-only-q");
    ok(&core, &mut s, "SET search_path = nosuch");
    ok(&core, &mut s, "TRUNCATE public.a, ONLY public.b");
    ok(&core, &mut s, "RESET search_path");
    assert_eq!(row_count(&core, &mut s, "a"), 0);
    assert_eq!(row_count(&core, &mut s, "b"), 0);
    assert_eq!(row_count(&core, &mut s, "\"in\""), 1);
}

/// 引用符付き識別子が式の語に見えても、索引 DDL の `ON` の直後の表名はゲートを素通りしない
#[test]
fn quoted_expression_word_does_not_hide_the_relation_after_an_introducer() {
    let (core, _g, mut s) = with_two_tables("sp-quoted-introducer");
    ok(&core, &mut s, "SET search_path = nosuch");
    for sql in [
        "CREATE INDEX \"is\" ON a (name)",
        "CREATE INDEX \"in\" ON b (name)",
    ] {
        assert_eq!(code(&core, &mut s, sql), "42P01", "{sql}");
    }
    ok(&core, &mut s, "RESET search_path");
    // 拒否された `CREATE INDEX` は索引を作っていない（同名で作れる）。
    ok(&core, &mut s, "CREATE INDEX \"is\" ON a (name)");
}

/// `public.` 修飾の CREATE VIEW／MATERIALIZED VIEW／SEQUENCE は作成先が文中で確定しているので、
/// `search_path` に `public` が無くても、`"$user"` と同名の SCHEMA があっても作れる。DROP も修飾
/// 名なら参照ゲートを通る。未修飾の同じ文は従来どおり拒否される（簡易クエリ）。
#[test]
fn public_qualified_object_ddl_ignores_the_path_in_simple_queries() {
    let (core, _g, mut s) = with_table("sp-obj-simple");
    ok(&core, &mut s, "SET search_path = nosuch");
    ok(&core, &mut s, "CREATE SEQUENCE public.q");
    ok(
        &core,
        &mut s,
        "CREATE VIEW public.v AS SELECT id FROM public.t",
    );
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW public.m AS SELECT id FROM public.t",
    );
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE q9"), "3F000");
    assert_eq!(code(&core, &mut s, "DROP SEQUENCE q"), "42P01");
    assert_eq!(code(&core, &mut s, "DROP VIEW v"), "42P01");
    ok(&core, &mut s, "DROP SEQUENCE public.q");
    ok(&core, &mut s, "DROP VIEW public.v");
    ok(&core, &mut s, "DROP MATERIALIZED VIEW public.m");

    // 自 username と同名の SCHEMA がある既定の path では、未修飾の作成はその SCHEMA へ行き（Issue #3196）、
    // 明示修飾の作成は path に依らず public へ行く。両者は独立している。
    ok(&core, &mut s, "RESET search_path");
    ok(&core, &mut s, "CREATE SCHEMA bob");
    s.set_session_user("bob");
    ok(&core, &mut s, "CREATE SEQUENCE q2");
    ok(&core, &mut s, "CREATE SEQUENCE public.q2");
    ok(&core, &mut s, "DROP SEQUENCE bob.q2");
    ok(&core, &mut s, "DROP SEQUENCE public.q2");
    ok(
        &core,
        &mut s,
        "CREATE VIEW public.v2 AS SELECT id FROM public.t",
    );
}

/// 拡張クエリ（Parse／Bind／Describe／Execute）でも、`public.` 修飾した CREATE／DROP は
/// 書き換え後のトークンでゲートを再実行されても `search_path` に影響されない。
#[test]
fn public_qualified_object_ddl_ignores_the_path_in_extended_queries() {
    let (core, _g, mut s) = with_table("sp-obj-ext");
    let c = ctx(TENANT);
    ok(&core, &mut s, "SET search_path = nosuch");
    for sql in [
        "CREATE SEQUENCE public.q",
        "CREATE VIEW public.v AS SELECT id FROM public.t",
        "CREATE MATERIALIZED VIEW public.m AS SELECT id FROM public.t",
    ] {
        let p = core
            .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
            .unwrap_or_else(|e| panic!("{sql}: parse {e}"));
        let b = core
            .bind_prepared_in_session(&s, &p, &[])
            .unwrap_or_else(|e| panic!("{sql}: bind {e}"));
        core.execute_parsed_in_session(&c, &mut s, &b)
            .unwrap_or_else(|e| panic!("{sql}: execute {} {e}", e.wire_code()));
    }
    for sql in [
        "DROP SEQUENCE public.q",
        "DROP VIEW public.v",
        "DROP MATERIALIZED VIEW public.m",
    ] {
        let p = core
            .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
            .unwrap_or_else(|e| panic!("{sql}: parse {e}"));
        // Describe も Bind と同じ再ゲートを通る。
        let _ = core.describe_prepared_in_session(&s, &p);
        let b = core
            .bind_prepared_in_session(&s, &p, &[])
            .unwrap_or_else(|e| panic!("{sql}: bind {e}"));
        core.execute_parsed_in_session(&c, &mut s, &b)
            .unwrap_or_else(|e| panic!("{sql}: execute {} {e}", e.wire_code()));
    }
    // 未修飾の同じ文は（DROP は Parse で、CREATE は作成先の判定で）拒否される。
    for (sql, want) in [
        ("CREATE SEQUENCE q9", "3F000"),
        ("DROP SEQUENCE q", "42P01"),
    ] {
        let e = core
            .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
            .and_then(|p| core.bind_prepared_in_session(&s, &p, &[]))
            .and_then(|b| core.execute_parsed_in_session(&c, &mut s.clone(), &b))
            .expect_err(sql);
        assert_eq!(e.wire_code(), want, "{sql}");
    }

    // 自 username と同名の SCHEMA がある既定の path でも明示修飾の作成は通る。
    ok(&core, &mut s, "RESET search_path");
    ok(&core, &mut s, "CREATE SCHEMA bob");
    s.set_session_user("bob");
    let p = core
        .parse_sql_prepared_with_declared_in_session(&s, "CREATE SEQUENCE public.q2", &[])
        .expect("parse");
    let b = core.bind_prepared_in_session(&s, &p, &[]).expect("bind");
    core.execute_parsed_in_session(&c, &mut s, &b)
        .expect("execute explicit public create");
}
// ---------------------------------------------------------------------------
// Issue #3195: 未修飾名の読み取りを `search_path` の自テナント SCHEMA で解決する。
// ポインタ: TABLE-27・TABLE-44・SQL-39・SQL-40・RLS-7・RLS-8・RLS-10、
// 設計記録 `docs/design/schema-tenant-namespace.md`（D5）。
// ---------------------------------------------------------------------------

/// SELECT の結果の先頭列（整数）を昇順に集める。
fn ids(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<i64> {
    let mut v: Vec<i64> = match ok(core, s, sql) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Integer(u) => i64::try_from(*u).expect("fits"),
                Cell::SignedInteger(i) => *i,
                other => panic!("{sql}: not an integer cell: {other:?}"),
            })
            .collect(),
        other => panic!("{sql}: expected Query, got {other:?}"),
    };
    v.sort_unstable();
    v
}

/// `public.t`（id 1）と `q1.t`（id 10・20）を持つ世界。
fn q1_world(label: &str) -> (EngineCore, CleanupGuard, SessionState) {
    let (core, g, mut s) = with_table(label);
    ok(&core, &mut s, "CREATE SCHEMA q1");
    ok(
        &core,
        &mut s,
        "CREATE TABLE q1.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO q1.t (id, name) VALUES (10, 'x')",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO q1.t (id, name) VALUES (20, 'y')",
    );
    (core, g, s)
}

#[test]
fn unqualified_reads_resolve_into_the_path_schema() {
    let (core, _g, mut s) = q1_world("sp-read-q1");
    ok(&core, &mut s, "SET search_path TO q1");
    // 修飾名 `q1.t` と同じ結果になる（SELECT・JOIN・IN サブクエリ・COUNT・CTE 内側）。
    for (unq, qual) in [
        (
            "SELECT id FROM t LIMIT 100",
            "SELECT id FROM q1.t LIMIT 100",
        ),
        (
            "SELECT a.id FROM t a JOIN t b ON a.id = b.id LIMIT 100",
            "SELECT a.id FROM q1.t a JOIN q1.t b ON a.id = b.id LIMIT 100",
        ),
        (
            "SELECT id FROM t WHERE id IN (SELECT id FROM t LIMIT 100) LIMIT 100",
            "SELECT id FROM q1.t WHERE id IN (SELECT id FROM q1.t LIMIT 100) LIMIT 100",
        ),
        ("SELECT COUNT(*) FROM t", "SELECT COUNT(*) FROM q1.t"),
        (
            "WITH c AS (SELECT id FROM t LIMIT 100) SELECT id FROM c LIMIT 100",
            "WITH c AS (SELECT id FROM q1.t LIMIT 100) SELECT id FROM c LIMIT 100",
        ),
    ] {
        let want = ok(&core, &mut s, qual);
        assert_eq!(ok(&core, &mut s, unq), want, "{unq}");
    }
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM t LIMIT 100"),
        vec![10, 20]
    );
    // 末尾 `;` 付きの簡易クエリ形（psql）。
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM t LIMIT 100;"),
        vec![10, 20]
    );
    // 明示トランザクション（`BEGIN` 時点のスナップショットで読む）でも同じ。
    let c = ctx(TENANT);
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    match core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT id FROM t LIMIT 100")
        .expect("select in txn")
    {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    // public は見えないので、q1 に無い名前は従来どおり `42P01`（利用者が書いた名前だけ）。
    let (c, msg) = err(&core, &mut s, "SELECT id FROM nosuch LIMIT 5");
    assert_eq!(c, "42P01");
    assert!(!msg.contains('#'), "{msg}");
}

/// 明示 Tx の `BEGIN` 後に別セッションが SCHEMA を改名・DROP しても、SCHEMA の ID 解決も存在判定と
/// 同じスナップショットで行い、public へ誤って落ちない（Issue #3195 レビュー指摘）。
#[test]
fn snapshot_txn_resolves_path_schema_from_the_begin_snapshot() {
    for ddl in ["ALTER SCHEMA q1 RENAME TO q9", "DROP SCHEMA q1 CASCADE"] {
        let (core, _g, mut s) = q1_world("sp-read-snap-schema");
        ok(&core, &mut s, "SET search_path TO q1, public");
        let c = ctx(TENANT);
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        // BEGIN 後に別セッションが q1 を変更して commit する。
        let mut other = ddl_session();
        ok(&core, &mut other, ddl);
        match core
            .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT id FROM t LIMIT 100")
            .expect("select in txn")
        {
            SqlOutcome::Query(r) => {
                let mut v: Vec<i64> = r
                    .rows
                    .iter()
                    .map(|row| match &row.cells[0] {
                        Cell::Integer(u) => i64::try_from(*u).expect("fits"),
                        Cell::SignedInteger(i) => *i,
                        other => panic!("not an integer cell: {other:?}"),
                    })
                    .collect();
                v.sort_unstable();
                assert_eq!(v, vec![10, 20], "{ddl}: must read the snapshot q1.t");
            }
            other => panic!("expected Query, got {other:?}"),
        }
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

/// 明示 Tx の `BEGIN` 後に別セッションが SCHEMA を改名・DROP しても、`COPY` の未修飾名は `BEGIN` 時点の
/// スナップショットで解決する。public の同名表（1 行）へは決して落ちない（Issue #3195 レビュー指摘・#3196）。
#[test]
fn copy_in_txn_resolves_path_schema_from_the_begin_snapshot() {
    for ddl in ["ALTER SCHEMA q1 RENAME TO q9", "DROP SCHEMA q1 CASCADE"] {
        let (core, _g, mut s) = q1_world("sp-copy-snap-schema");
        ok(&core, &mut s, "SET search_path TO q1, public");
        let c = ctx(TENANT);
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let mut other = ddl_session();
        ok(&core, &mut other, ddl);
        match core.begin_copy_in_txn(&c, &s, &mut txn, "COPY t TO STDOUT") {
            Ok(engine::core::CopyPlan::To(_, r, _)) => {
                let mut v: Vec<String> = r
                    .rows
                    .iter()
                    .map(|row| format!("{:?}", row.cells[0]))
                    .collect();
                v.sort_unstable();
                assert_eq!(v.len(), 2, "{ddl}: must read the snapshot q1.t, got {v:?}");
            }
            Ok(_) => panic!("{ddl}: expected a COPY TO plan"),
            Err(e) => {
                // 実装上 BEGIN 時点で読めない形は `42P01`（public.t の行へは落ちない）。
                assert_eq!(e.wire_code(), "42P01", "{ddl}: {e}");
            }
        }
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

#[test]
fn path_elements_are_resolved_in_order() {
    let (core, _g, mut s) = q1_world("sp-read-order");
    ok(
        &core,
        &mut s,
        "CREATE TABLE q1.only_q1 (id BIGINT PRIMARY KEY)",
    );
    ok(&core, &mut s, "INSERT INTO q1.only_q1 (id) VALUES (77)");
    ok(&core, &mut s, "CREATE SCHEMA q2");
    ok(&core, &mut s, "CREATE TABLE q2.t (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "INSERT INTO q2.t (id) VALUES (30)");
    ok(&core, &mut s, "CREATE SCHEMA empty_s");

    ok(&core, &mut s, "SET search_path TO q1, public");
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM t LIMIT 100"),
        vec![10, 20]
    );
    ok(&core, &mut s, "SET search_path TO public, q1");
    assert_eq!(ids(&core, &mut s, "SELECT id FROM t LIMIT 100"), vec![1]);
    // public に無い名前は後ろの q1 で見つかる。
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM only_q1 LIMIT 100"),
        vec![77]
    );
    // 先勝ち。
    ok(&core, &mut s, "SET search_path TO q2, q1");
    assert_eq!(ids(&core, &mut s, "SELECT id FROM t LIMIT 100"), vec![30]);
    ok(&core, &mut s, "SET search_path TO q1, q2");
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM t LIMIT 100"),
        vec![10, 20]
    );
    // 空の SCHEMA・不在の名前は飛ばす。
    ok(
        &core,
        &mut s,
        "SET search_path TO empty_s, nosuch, q2, public",
    );
    assert_eq!(ids(&core, &mut s, "SELECT id FROM t LIMIT 100"), vec![30]);
    // 組み込み SCHEMA の要素は飛ばす。
    ok(&core, &mut s, "SET search_path TO pg_catalog, q1, public");
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM t LIMIT 100"),
        vec![10, 20]
    );
}

#[test]
fn temp_tables_win_over_path_schemas() {
    let (core, _g, mut s) = q1_world("sp-read-temp");
    ok(&core, &mut s, "SET search_path TO q1, public");
    ok(&core, &mut s, "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "INSERT INTO t (id) VALUES (555)");
    assert_eq!(ids(&core, &mut s, "SELECT id FROM t LIMIT 100"), vec![555]);
    // 修飾名は一時表の影響を受けない。
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM q1.t LIMIT 100"),
        vec![10, 20]
    );
}

#[test]
fn views_resolve_through_the_path() {
    let (core, _g, mut s) = q1_world("sp-read-view");
    ok(
        &core,
        &mut s,
        "CREATE VIEW q1.vw AS SELECT id FROM q1.t LIMIT 100",
    );
    ok(&core, &mut s, "SET search_path TO q1, public");
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM vw LIMIT 100"),
        vec![10, 20]
    );
    // public に同名のビューがあっても、path の先頭の SCHEMA が優先される。
    ok(
        &core,
        &mut s,
        "CREATE VIEW public.vw AS SELECT id FROM public.t LIMIT 100",
    );
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM vw LIMIT 100"),
        vec![10, 20]
    );
    ok(&core, &mut s, "SET search_path TO public, q1");
    assert_eq!(ids(&core, &mut s, "SELECT id FROM vw LIMIT 100"), vec![1]);
}

/// SEQUENCE の未修飾の文字列引数（`DEFAULT nextval('q')`）も path で解決する。作成先（表）も path の
/// 先頭の SCHEMA になる（Issue #3196）。
#[test]
fn sequence_literals_follow_the_path() {
    let (core, _g, mut s) = q1_world("sp-seq-literal");
    ok(&core, &mut s, "CREATE SEQUENCE q1.q START 100");
    ok(&core, &mut s, "CREATE SEQUENCE public.q START 5");
    ok(&core, &mut s, "SET search_path TO q1, public");
    ok(
        &core,
        &mut s,
        "CREATE TABLE w (id BIGINT PRIMARY KEY, n BIGINT DEFAULT nextval('q'))",
    );
    // w は q1 に作られ、採番は q1.q の 100。
    ok(&core, &mut s, "INSERT INTO w (id) VALUES (1)");
    assert_eq!(ids(&core, &mut s, "SELECT n FROM q1.w LIMIT 5"), vec![100]);
    assert_eq!(
        code(&core, &mut s, "SELECT n FROM public.w LIMIT 5"),
        "42P01"
    );
    ok(&core, &mut s, "SET search_path TO public, q1");
    ok(
        &core,
        &mut s,
        "CREATE TABLE w2 (id BIGINT PRIMARY KEY, n BIGINT DEFAULT nextval('q'))",
    );
    ok(&core, &mut s, "INSERT INTO w2 (id) VALUES (1)");
    assert_eq!(ids(&core, &mut s, "SELECT n FROM w2 LIMIT 5"), vec![5]);
}

#[test]
fn dollar_user_schema_is_searched_first_on_the_default_path() {
    let (core, _g) = new_core("sp-read-user");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE TABLE t (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "INSERT INTO t (id) VALUES (1)");
    ok(&core, &mut s, "CREATE SCHEMA alice");
    ok(
        &core,
        &mut s,
        "CREATE TABLE alice.t (id BIGINT PRIMARY KEY)",
    );
    ok(&core, &mut s, "INSERT INTO alice.t (id) VALUES (2)");
    ok(
        &core,
        &mut s,
        "CREATE TABLE only_pub (id BIGINT PRIMARY KEY)",
    );
    ok(&core, &mut s, "INSERT INTO only_pub (id) VALUES (9)");
    // ユーザー未設定（Rust API）は `$user` を飛ばすので public。
    assert_eq!(ids(&core, &mut s, "SELECT id FROM t LIMIT 100"), vec![1]);
    // username と同名の SCHEMA は既定の path で先に見つかる。
    s.set_session_user("alice");
    assert_eq!(ids(&core, &mut s, "SELECT id FROM t LIMIT 100"), vec![2]);
    // SCHEMA に無い名前は public へ落ちる。
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM only_pub LIMIT 100"),
        vec![9]
    );
    // 別ユーザーは同名の SCHEMA が無いので public。
    let mut other = ddl_session();
    other.set_session_user("carol");
    assert_eq!(
        ids(&core, &mut other, "SELECT id FROM t LIMIT 100"),
        vec![1]
    );
}

/// 読み取り以外の文（DML・TRUNCATE・DROP・データ変更 CTE・COPY）も、未修飾名を path の要素順に自テナントの
/// SCHEMA まで解決し、修飾名 `q1.t` と同じ対象に効く（Issue #3196）。public.t の行・定義は変わらない。
#[test]
fn non_read_statements_resolve_into_the_path_schema() {
    let (core, _g, mut s) = q1_world("sp-write-q1");
    ok(&core, &mut s, "SET search_path TO q1, public");
    ok(&core, &mut s, "INSERT INTO t (id, name) VALUES (2, 'z')");
    ok(&core, &mut s, "UPDATE t SET name = 'u' WHERE id = 10");
    ok(&core, &mut s, "DELETE FROM t WHERE id = 20");
    ok(
        &core,
        &mut s,
        "WITH x AS (INSERT INTO t (id, name) VALUES (3, 'w') RETURNING id) SELECT id FROM x",
    );
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM q1.t LIMIT 100"),
        vec![2, 3, 10]
    );
    // public.t は変わらない。
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM public.t LIMIT 100"),
        vec![1]
    );
    // COPY TO は q1.t を読む。
    match core
        .begin_copy(
            &ctx(TENANT),
            &s,
            "COPY (SELECT id FROM t LIMIT 5) TO STDOUT",
        )
        .expect("COPY resolves into q1")
    {
        engine::core::CopyPlan::To(_, r, _) => assert_eq!(r.rows.len(), 3),
        _ => panic!("expected a COPY TO plan"),
    }
    ok(&core, &mut s, "TRUNCATE t");
    assert!(ids(&core, &mut s, "SELECT id FROM q1.t LIMIT 100").is_empty());
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM public.t LIMIT 100"),
        vec![1]
    );
    ok(&core, &mut s, "DROP TABLE t");
    assert_eq!(code(&core, &mut s, "SELECT id FROM q1.t LIMIT 5"), "42P01");
    // public.t は残っている（path 上に q1.t が無くなったので、未修飾の `t` は public へ落ちる）。
    assert_eq!(ids(&core, &mut s, "SELECT id FROM t LIMIT 100"), vec![1]);
}

/// 明示修飾 `public.t` の `COPY` は、path の前方に同名の `q1.t` があっても public.t を対象にする
/// （修飾を外した後で未修飾名として再解決しない。Issue #3196）。自動コミット・明示 Tx の両方で、
/// `TO STDOUT`・`FROM STDIN`・`COPY (SELECT ...)` の 3 形を確かめる。
#[test]
fn copy_keeps_explicit_public_qualification() {
    let (core, _g, mut s) = q1_world("sp-copy-public-qualified");
    ok(&core, &mut s, "SET search_path TO q1, public");
    let c = ctx(TENANT);
    let rows = |plan: engine::core::CopyPlan| match plan {
        engine::core::CopyPlan::To(_, r, _) => r.rows.len(),
        _ => panic!("expected a COPY TO plan"),
    };
    let table = |plan: engine::core::CopyPlan| match plan {
        engine::core::CopyPlan::From(sess) => sess.table().to_string(),
        _ => panic!("expected a COPY FROM plan"),
    };
    let to_sqls = [
        ("COPY public.t TO STDOUT", 1usize),
        ("COPY (SELECT id FROM public.t LIMIT 5) TO STDOUT", 1),
        ("COPY t TO STDOUT", 2),
        ("COPY (SELECT id FROM t LIMIT 5) TO STDOUT", 2),
    ];
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    for (sql, n) in to_sqls {
        assert_eq!(rows(core.begin_copy(&c, &s, sql).expect(sql)), n, "{sql}");
        assert_eq!(
            rows(core.begin_copy_in_txn(&c, &s, &mut txn, sql).expect(sql)),
            n,
            "txn {sql}"
        );
    }
    let public_from = table(
        core.begin_copy(&c, &s, "COPY public.t (id, name) FROM STDIN")
            .expect("copy from public"),
    );
    let bare_from = table(
        core.begin_copy(&c, &s, "COPY t (id, name) FROM STDIN")
            .expect("copy from path"),
    );
    assert_ne!(public_from, bare_from, "public.t must not resolve to q1.t");
    let public_txn = table(
        core.begin_copy_in_txn(&c, &s, &mut txn, "COPY public.t (id, name) FROM STDIN")
            .expect("txn copy from public"),
    );
    assert_eq!(public_txn, public_from);
}

/// `CREATE TEMP TABLE t (.. REFERENCES t ..)` の自己参照は、path 前方の永続表 `q1.t` ではなく新しい
/// 一時表を指す（Issue #3196）。
#[test]
fn temp_table_self_reference_ignores_persistent_table_in_path() {
    let (core, _g, mut s) = q1_world("sp-temp-self-fk");
    ok(&core, &mut s, "SET search_path TO q1, public");
    // 自己参照は一時表同士の FK として扱われ（未対応で `0A000`）、q1.t への FK へ化けない。
    let (c, msg) = err(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, parent BIGINT REFERENCES t (id))",
    );
    assert_eq!(c, "0A000");
    assert!(msg.contains("between temporary tables"), "{msg}");
}

/// 未修飾の作成・DML・DDL が、path の先頭の SCHEMA だけで完結する（public に同名が無い構成）。
#[test]
fn create_and_dml_complete_inside_the_path_schema() {
    let (core, _g, mut s) = q1_world("sp-create-q1-only");
    ok(&core, &mut s, "SET search_path TO q1");
    ok(
        &core,
        &mut s,
        "CREATE TABLE t2 (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO t2 (id, name) VALUES (1, 'a')");
    ok(&core, &mut s, "INSERT INTO t2 (id, name) VALUES (2, 'b')");
    ok(&core, &mut s, "UPDATE t2 SET name = 'c' WHERE id = 1");
    ok(&core, &mut s, "DELETE FROM t2 WHERE id = 2");
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM q1.t2 LIMIT 100"),
        vec![1]
    );
    assert_eq!(ids(&core, &mut s, "SELECT id FROM t2 LIMIT 100"), vec![1]);
    // public には作られていない。
    assert_eq!(
        code(&core, &mut s, "SELECT id FROM public.t2 LIMIT 5"),
        "42P01"
    );
    // 索引・列追加・ビュー・CTAS・UPDATE FROM／DELETE USING・ON CONFLICT も同じ SCHEMA に効く。
    ok(&core, &mut s, "CREATE INDEX i2 ON t2 (name)");
    ok(&core, &mut s, "ALTER TABLE t2 ADD COLUMN extra BIGINT");
    ok(&core, &mut s, "CREATE VIEW v2 AS SELECT id FROM t2");
    ok(&core, &mut s, "CREATE TABLE t3 AS SELECT id FROM t2");
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM q1.v2 LIMIT 100"),
        vec![1]
    );
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM q1.t3 LIMIT 100"),
        vec![1]
    );
    ok(
        &core,
        &mut s,
        "UPDATE t2 SET extra = 7 FROM t3 WHERE t2.id = t3.id",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO t2 (id, name) VALUES (1, 'dup') ON CONFLICT (id) DO UPDATE SET name = 'upd'",
    );
    ok(&core, &mut s, "DELETE FROM t3 USING t2 WHERE t3.id = t2.id");
    assert!(ids(&core, &mut s, "SELECT id FROM q1.t3 LIMIT 100").is_empty());
    ok(&core, &mut s, "DROP INDEX i2");
    ok(&core, &mut s, "DROP TABLE t3");
    // 重複作成のエラー文言に内部 ID を出さない。
    let (c, msg) = err(&core, &mut s, "CREATE TABLE t2 (id BIGINT PRIMARY KEY)");
    assert_eq!(c, "42P07");
    assert!(!msg.contains('#'), "{msg}");
    ok(
        &core,
        &mut s,
        "CREATE TABLE IF NOT EXISTS t2 (id BIGINT PRIMARY KEY)",
    );
}

/// 自己参照 FK は、修飾形でも未修飾形でも同じく受理される（Issue #3178。親が宣言した `id` 列の参照は
/// DDL 束縛で通る）。未修飾形は `search_path` の作成先 SCHEMA（`q1`）へ書き換わった自己参照になる。
/// 書き換え自体は `sql::search_path` の単体テストでも固定している。
#[test]
fn self_referencing_foreign_key_behaves_like_the_qualified_form() {
    let (core, _g, mut s) = q1_world("sp-self-fk");
    ok(
        &core,
        &mut s,
        "CREATE TABLE q1.t3 (id BIGINT PRIMARY KEY, parent BIGINT REFERENCES q1.t3 (id))",
    );
    ok(&core, &mut s, "SET search_path TO q1, public");
    ok(
        &core,
        &mut s,
        "CREATE TABLE t4 (id BIGINT PRIMARY KEY, parent BIGINT REFERENCES t4 (id))",
    );
    // 未修飾の t4 は作成先の q1 に実在する。
    ok(&core, &mut s, "SELECT id FROM q1.t4");
}

/// 作成・DML は自テナントの SCHEMA にだけ解決される。他テナントの同名 SCHEMA は不在と同じで、
/// 文言に内部 ID を出さない。
#[test]
fn other_tenant_with_the_same_path_cannot_create_or_write_into_the_schema() {
    let (core, _g, mut s) = q1_world("sp-write-other-tenant");
    let mut bob = ddl_session();
    let bob_ctx = ctx("bob");
    core.execute_sql_in_session(&bob_ctx, &mut bob, "SET search_path TO q1")
        .expect("set");
    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY)",
        "CREATE SEQUENCE sq",
    ] {
        let e = core
            .execute_sql_in_session(&bob_ctx, &mut bob, sql)
            .expect_err(sql);
        assert_eq!(e.wire_code(), "3F000", "{sql}");
        assert!(!e.to_string().contains('#'), "{sql}: {e}");
    }
    let e = core
        .execute_sql_in_session(
            &bob_ctx,
            &mut bob,
            "INSERT INTO t (id, name) VALUES (5, 'b')",
        )
        .expect_err("insert");
    assert_eq!(e.wire_code(), "42P01");
    assert!(!e.to_string().contains('#'));
    // alice の q1.t は変わらない。
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM q1.t LIMIT 100"),
        vec![10, 20]
    );
}

/// 明示トランザクション内の DDL は、path 形でも従来どおり `0A000`（この前提を固定する）。
#[test]
fn ddl_in_an_explicit_transaction_stays_unsupported_with_a_path_schema() {
    let (core, _g, mut s) = q1_world("sp-ddl-in-txn");
    ok(&core, &mut s, "SET search_path TO q1, public");
    let c = ctx(TENANT);
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(
            &c,
            &mut s,
            &mut txn,
            "CREATE TABLE tx1 (id BIGINT PRIMARY KEY)",
        )
        .expect_err("DDL in txn");
    assert_eq!(e.wire_code(), "0A000");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// `ALTER TABLE <未修飾> SET SCHEMA ..` で対象が SCHEMA に解決される形は固定文言の `0A000`。
// tracked: #3198
#[test]
fn set_schema_on_a_path_resolved_table_is_unsupported() {
    let (core, _g, mut s) = q1_world("sp-set-schema");
    ok(&core, &mut s, "SET search_path TO q1, public");
    let (c, msg) = err(&core, &mut s, "ALTER TABLE t SET SCHEMA public");
    assert_eq!(c, "0A000");
    assert!(!msg.contains('#'), "{msg}");
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM q1.t LIMIT 100"),
        vec![10, 20]
    );
}

#[test]
fn unprivileged_ddl_and_unbound_grant_keep_42501_ahead_of_resolution() {
    let (core, _g, mut s) = q1_world("sp-order-42501");
    ok(&core, &mut s, "SET search_path TO q1, public");
    // DDL 権限の無いセッション（同じ path・同じテナント）。
    let mut plain = SessionState::default();
    ok(&core, &mut plain, "SET search_path TO q1, public");
    assert_eq!(code(&core, &mut plain, "DROP TABLE t"), "42501");
    assert_eq!(
        code(&core, &mut plain, "CREATE TABLE w (id BIGINT PRIMARY KEY)"),
        "42501"
    );
    assert_eq!(code(&core, &mut plain, "CREATE SEQUENCE w"), "42501");
    assert_eq!(
        code(&core, &mut plain, "GRANT SELECT ON t TO alice"),
        "42501"
    );
}

/// 準備済み文を Bind して実行し、先頭列の整数を昇順で返す（拡張クエリの Bind→Execute 相当）。
fn run_prepared(
    core: &EngineCore,
    s: &mut SessionState,
    p: &engine::core::PreparedSql,
    values: &[Option<Vec<u8>>],
) -> Result<Vec<i64>, engine::sql::allowlist::SqlSurfaceError> {
    let bound = core.bind_prepared_in_session(s, p, values)?;
    let mut v: Vec<i64> = match core.execute_parsed_in_session(&ctx(TENANT), s, &bound)? {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Integer(u) => i64::try_from(*u).expect("fits"),
                Cell::SignedInteger(i) => *i,
                other => panic!("not an integer cell: {other:?}"),
            })
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    };
    v.sort_unstable();
    Ok(v)
}

fn parse_ext(
    core: &EngineCore,
    s: &SessionState,
    sql: &str,
) -> Result<engine::core::PreparedSql, engine::sql::allowlist::SqlSurfaceError> {
    core.parse_sql_prepared_with_declared_in_session(s, sql, &[])
}

/// Issue #3197: 拡張クエリは Parse・Bind・Describe の時点の `search_path` で未修飾名を解決し直す。
/// 簡易クエリと同じ形（SCHEMA に解決される Parse）を受理し、`$n` あり・なしの両方で path の往復に追従する。
#[test]
fn extended_query_re_resolves_references_after_search_path_changes() {
    let (core, _g, mut s) = q1_world("sp-ext-reresolve");
    // wire-server は handshake で解決テナントを束縛する。
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "SET search_path TO public");
    let plain = parse_ext(&core, &s, "SELECT id FROM t LIMIT 5").expect("parse under public");
    let with_param =
        parse_ext(&core, &s, "SELECT id FROM t WHERE id > $1 LIMIT 5").expect("parse with param");
    let zero = Some(b"0".to_vec());
    assert_eq!(
        run_prepared(&core, &mut s, &plain, &[]).expect("public"),
        vec![1]
    );
    assert_eq!(
        run_prepared(&core, &mut s, &with_param, std::slice::from_ref(&zero)).expect("public"),
        vec![1]
    );

    // path を変えると、同じ準備済み文が q1.t へ解決される（Bind・Describe・Execute）。
    ok(&core, &mut s, "SET search_path TO q1, public");
    assert_eq!(
        run_prepared(&core, &mut s, &plain, &[]).expect("q1"),
        vec![10, 20]
    );
    assert_eq!(
        run_prepared(&core, &mut s, &with_param, std::slice::from_ref(&zero)).expect("q1"),
        vec![10, 20]
    );
    let cols = core
        .describe_prepared_in_session(&s, &plain)
        .expect("describe after SET");
    assert_eq!(cols.map(|c| c.len()), Some(1));

    // SCHEMA に解決される Parse も受理される（簡易クエリと同じ解決）。
    let in_q1 = parse_ext(&core, &s, "SELECT id FROM t LIMIT 5").expect("parse resolving");
    assert_eq!(
        run_prepared(&core, &mut s, &in_q1, &[]).expect("q1"),
        vec![10, 20]
    );
    // 構文エラーは優先する。
    let e = parse_ext(&core, &s, "SELECT FROM FROM t").expect_err("syntax first");
    assert_eq!(e.wire_code(), "42601");

    // public だけの path へ戻せば public.t へ戻る（q1 で Parse した文も追従する）。
    ok(&core, &mut s, "SET search_path TO public");
    assert_eq!(
        run_prepared(&core, &mut s, &plain, &[]).expect("public"),
        vec![1]
    );
    assert_eq!(
        run_prepared(&core, &mut s, &in_q1, &[]).expect("public"),
        vec![1]
    );
    assert_eq!(
        run_prepared(&core, &mut s, &with_param, &[zero]).expect("public"),
        vec![1]
    );
}

/// epoch が変わっても解決が同じなら使い回し、ROLLBACK で path が戻れば古い解決結果を使わない。
#[test]
fn extended_query_epoch_mismatch_never_reuses_a_stale_resolution() {
    let (core, _g, mut s) = q1_world("sp-ext-epoch");
    assert!(s.bind_resolution_tenant(TENANT));
    let c = ctx(TENANT);
    // 往復して epoch だけが変わる（解決は同じ）。
    let plain = parse_ext(&core, &s, "SELECT id FROM t LIMIT 5").expect("parse");
    ok(&core, &mut s, "SET search_path TO q1, public");
    ok(&core, &mut s, "SET search_path TO public");
    assert_eq!(
        run_prepared(&core, &mut s, &plain, &[]).expect("same"),
        vec![1]
    );
    // BEGIN; SET search_path TO q1; Parse; ROLLBACK 後の Bind は public へ解決される。
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "SET search_path TO q1")
        .expect("set in txn");
    let in_q1 = parse_ext(&core, &s, "SELECT id FROM t LIMIT 5").expect("parse in txn");
    assert_eq!(
        run_prepared(&core, &mut s, &in_q1, &[]).expect("q1"),
        vec![10, 20]
    );
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(
        run_prepared(&core, &mut s, &in_q1, &[]).expect("rolled back"),
        vec![1]
    );
}

/// 同じ epoch でも、Parse の後に作られた表（カタログの変化）で解決先が変われば再解決する。
#[test]
fn extended_query_re_resolves_when_the_catalog_changes_within_one_epoch() {
    let (core, _g, mut s) = q1_world("sp-ext-catalog");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "CREATE SCHEMA q3");
    ok(&core, &mut s, "SET search_path TO q3, public");
    let plain = parse_ext(&core, &s, "SELECT id FROM t LIMIT 5").expect("parse");
    assert_eq!(
        run_prepared(&core, &mut s, &plain, &[]).expect("public"),
        vec![1]
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE q3.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO q3.t (id, name) VALUES (30, 'z')",
    );
    assert_eq!(
        run_prepared(&core, &mut s, &plain, &[]).expect("q3"),
        vec![30]
    );
}

/// 再 Parse で `$n` の型・結果列が変わる場合は、公告済みの型と食い違う文を実行せず固定文言の `0A000`。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn extended_query_rejects_a_re_resolution_that_changes_parameter_or_result_types() {
    let (core, _g, mut s) = q1_world("sp-ext-type-change");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "CREATE SCHEMA q2");
    ok(
        &core,
        &mut s,
        "CREATE TABLE q2.t (id BIGINT PRIMARY KEY, name BIGINT)",
    );
    ok(&core, &mut s, "SET search_path TO public");
    let by_param = parse_ext(&core, &s, "SELECT id FROM t WHERE name = $1 LIMIT 5").expect("parse");
    let cols = parse_ext(&core, &s, "SELECT name FROM t LIMIT 5").expect("parse cols");
    ok(&core, &mut s, "SET search_path TO q2, public");
    let e = core
        .bind_prepared_in_session(&s, &by_param, &[Some(b"1".to_vec())])
        .expect_err("parameter type changed");
    assert_eq!(e.wire_code(), "0A000");
    assert!(!e.to_string().contains('#'), "{e}");
    let e = core
        .bind_prepared_in_session(&s, &cols, &[])
        .expect_err("result column type changed");
    assert_eq!(e.wire_code(), "0A000");
    let e = core
        .describe_prepared_in_session(&s, &cols)
        .expect_err("describe also fails closed");
    assert_eq!(e.wire_code(), "0A000");
    assert!(!e.to_string().contains('#'), "{e}");
}

/// DML・作成系 DDL も拡張クエリで path の自テナント SCHEMA に解決される。
#[test]
fn extended_query_dml_and_ddl_resolve_into_the_path_schema() {
    let (core, _g, mut s) = q1_world("sp-ext-dml");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "SET search_path TO q1, public");
    let run = |core: &EngineCore, s: &mut SessionState, sql: &str, v: &[Option<Vec<u8>>]| {
        let p = parse_ext(core, s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let b = core
            .bind_prepared_in_session(s, &p, v)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        core.execute_parsed_in_session(&ctx(TENANT), s, &b)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    };
    run(
        &core,
        &mut s,
        "INSERT INTO t (id, name) VALUES ($1, 'n')",
        &[Some(b"11".to_vec())],
    );
    run(
        &core,
        &mut s,
        "UPDATE t SET name = 'm' WHERE id = $1",
        &[Some(b"11".to_vec())],
    );
    run(&core, &mut s, "DELETE FROM t WHERE id = 20", &[]);
    run(
        &core,
        &mut s,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, name TEXT)",
        &[],
    );
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM q1.t LIMIT 100"),
        vec![10, 11]
    );
    // 作成先は path の先頭の q1。public 側には作られない。
    ok(&core, &mut s, "SELECT id FROM q1.u LIMIT 1");
    ok(&core, &mut s, "SET search_path TO public");
    assert_eq!(code(&core, &mut s, "SELECT id FROM u LIMIT 1"), "42P01");
    assert_eq!(ids(&core, &mut s, "SELECT id FROM t LIMIT 100"), vec![1]);
}

/// 別テナントの SCHEMA には解決されず、エラー文言にも内部キーや存在情報を含めない。
#[test]
fn extended_query_does_not_resolve_into_another_tenants_schema() {
    let (core, _g, mut a) = q1_world("sp-ext-boundary");
    assert!(a.bind_resolution_tenant(TENANT));
    // 別テナント bob は q1 を持たない。
    let mut b = ddl_session();
    assert!(b.bind_resolution_tenant("bob"));
    let bob = ctx("bob");
    core.execute_sql_in_session(&bob, &mut b, "SET search_path TO q1, nosuch")
        .expect("bob set");
    let e = parse_ext(&core, &b, "SELECT id FROM t LIMIT 5").expect_err("hidden public");
    assert_eq!(e.wire_code(), "42P01");
    assert!(!e.to_string().contains('#'), "{e}");
    core.execute_sql_in_session(&bob, &mut b, "SET search_path TO q1, public")
        .expect("bob set 2");
    let p = parse_ext(&core, &b, "SELECT id FROM t LIMIT 5").expect("bob parse");
    let bound = core
        .bind_prepared_in_session(&b, &p, &[])
        .expect("bob bind");
    match core
        .execute_parsed_in_session(&bob, &mut b, &bound)
        .expect("bob exec")
    {
        // 解決先は共有の public.t（alice の q1.t の行は決して返らない）。
        SqlOutcome::Query(r) => assert!(
            r.rows.iter().all(|row| !matches!(
                &row.cells[0],
                Cell::Integer(10 | 20) | Cell::SignedInteger(10 | 20)
            )),
            "bob must never see alice's q1.t rows"
        ),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// セッションを持たない Parse の文は `search_path` を知らないため、解決が有効なセッションでは `0A000`（安全弁）。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn sessionless_prepared_statements_stay_rejected_when_a_path_schema_resolves() {
    let (core, _g, mut s) = q1_world("sp-ext-sessionless");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "SET search_path TO q1, public");
    let p = core
        .parse_sql_prepared_with_declared("SELECT id FROM t LIMIT 5", &[])
        .expect("sessionless parse");
    let e = core
        .bind_prepared_in_session(&s, &p, &[])
        .expect_err("sessionless statement");
    assert_eq!(e.wire_code(), "0A000");
}
/// Issue #3197: 拡張クエリの `COPY` も Parse・Bind で受理される。`COPY` は Execute で元の SQL を現在の path へ
/// 再解決するため、Bind 後に path が変わった（Bind で検証した宛先とずれる）portal は `0A000` で拒否する。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn extended_copy_is_accepted_and_rejects_a_path_change_after_bind() {
    let (core, _g, mut s) = q1_world("sp-ext-copy");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "SET search_path TO q1, public");
    let copy = parse_ext(&core, &s, "COPY t TO STDOUT").expect("extended COPY parses");
    let bound = core
        .bind_prepared_in_session(&s, &copy, &[])
        .expect("extended COPY binds");
    // path が同じなら Execute へ進める。
    bound.require_session_current(&s).expect("same path");
    // Bind 後に path が変わると、Bind で検証した宛先と食い違うため拒否する。
    ok(&core, &mut s, "SET search_path TO public");
    let e = bound
        .require_session_current(&s)
        .expect_err("path changed after bind");
    assert_eq!(e.wire_code(), "0A000");
    // public だけの path で Bind した文も、その後 q1 が見える path へ変わったら拒否する。
    let bound_public = core
        .bind_prepared_in_session(&s, &copy, &[])
        .expect("rebind under public");
    ok(&core, &mut s, "SET search_path TO q1, public");
    let e = bound_public
        .require_session_current(&s)
        .expect_err("path widened after bind");
    assert_eq!(e.wire_code(), "0A000");
}

/// Issue #3197: `COPY` の Bind 後、同じ `search_path` のまま path の前方へ同名の表が作られると、Execute は
/// Bind で検証した宛先とは別の関係へ解決してしまう。Bind 時の宛先を記録して照合し、`0A000` で拒否する。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn extended_copy_rejects_a_same_name_relation_created_ahead_in_path_after_bind() {
    let (core, _g, mut s) = q1_world("sp-ext-copy-pin");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "CREATE SCHEMA q2");
    ok(&core, &mut s, "SET search_path TO q2, public");
    let copy = parse_ext(&core, &s, "COPY t TO STDOUT").expect("extended COPY parses");
    let bound = core
        .bind_prepared_in_session(&s, &copy, &[])
        .expect("extended COPY binds");
    let txn = core.new_session_transaction();
    bound.require_session_current(&s).expect("same epoch");
    core.require_copy_target_current(&s, &txn, &bound)
        .expect("target unchanged");
    // 同じ世代のまま、path の前方（q2）へ同名の表が作られる。
    ok(&core, &mut s, "CREATE TABLE q2.t (id BIGINT PRIMARY KEY)");
    bound
        .require_session_current(&s)
        .expect("epoch alone cannot see it");
    let e = core
        .require_copy_target_current(&s, &txn, &bound)
        .expect_err("target retargeted");
    assert_eq!(e.wire_code(), "0A000");
}

/// Issue #3197: 照合した宛先は `COPY` 開始へそのまま渡る。照合の後、同じ世代のまま path の前方（q2）へ同名の表が
/// 作られても、`COPY` 開始は検証した `public.t`（1 行）を読み、後から作られた `q2.t`（3 行）へ化けない。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn extended_copy_start_uses_the_verified_target_not_a_table_created_after_the_check() {
    let (core, _g, mut s) = q1_world("sp-ext-copy-armed");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "CREATE SCHEMA q2");
    ok(&core, &mut s, "SET search_path TO q2, public");
    let copy = parse_ext(&core, &s, "COPY t TO STDOUT").expect("extended COPY parses");
    let bound = core
        .bind_prepared_in_session(&s, &copy, &[])
        .expect("extended COPY binds");
    let mut txn = core.new_session_transaction();
    core.arm_copy_target(&mut s, &txn, &bound)
        .expect("target unchanged");
    ok(&core, &mut s, "CREATE TABLE q2.t (id BIGINT PRIMARY KEY)");
    for id in [30, 31, 32] {
        ok(
            &core,
            &mut s,
            &format!("INSERT INTO q2.t (id) VALUES ({id})"),
        );
    }
    let copy_sql = "COPY t TO STDOUT";
    match core.begin_copy_in_txn_armed(&ctx(TENANT), &mut s, &mut txn, copy_sql) {
        Ok(engine::core::CopyPlan::To(_, r, _)) => {
            assert_eq!(r.rows.len(), 1, "must read the verified public.t");
        }
        Ok(_) => panic!("expected a COPY TO plan"),
        Err(e) => panic!("armed COPY must start: {e}"),
    }
    // 置かれた宛先は 1 回で消える。次の同じ文は現在の path で再解決する（q2.t の 3 行）。
    match core.begin_copy_in_txn_armed(&ctx(TENANT), &mut s, &mut txn, copy_sql) {
        Ok(engine::core::CopyPlan::To(_, r, _)) => assert_eq!(r.rows.len(), 3),
        Ok(_) => panic!("expected a COPY TO plan"),
        Err(e) => panic!("unarmed COPY must start: {e}"),
    }
}

/// Issue #3197: public の表へ解決した準備済み文は、path から `public` が消えて名前がどの要素にも無くなれば、
/// Bind だけでなく Describe(S) でも `42P01` になる（public の表の列を公告し続けない）。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn extended_query_describe_rejects_miss_after_public_dropped_from_path() {
    let (core, _g, mut s) = q1_world("sp-ext-describe-miss");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "SET search_path TO public");
    let plain = parse_ext(&core, &s, "SELECT id FROM t LIMIT 5").expect("parse under public");
    ok(&core, &mut s, "DROP TABLE q1.t");
    ok(&core, &mut s, "SET search_path TO q1");
    let e = core
        .describe_prepared_in_session(&s, &plain)
        .expect_err("describe must fail on miss");
    assert_eq!(e.wire_code(), "42P01");
    let e = run_prepared(&core, &mut s, &plain, &[]).expect_err("bind must fail on miss");
    assert_eq!(e.wire_code(), "42P01");
}

/// Issue #3197: 再解決後の結果列は、Parse 時に公告した列と比べる。旧参照先（q1.t）が DROP されても、
/// 解決先（public.t）の結果列が同じなら通る（現在のカタログから旧側を再導出して常に拒否しない）。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn extended_query_compares_against_advertised_columns_after_old_target_dropped() {
    let (core, _g, mut s) = q1_world("sp-ext-cols-dropped");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "SET search_path TO q1, public");
    let plain = parse_ext(&core, &s, "SELECT id FROM t LIMIT 5").expect("parse under q1");
    assert_eq!(
        run_prepared(&core, &mut s, &plain, &[]).expect("q1"),
        vec![10, 20]
    );
    ok(&core, &mut s, "DROP TABLE q1.t");
    assert_eq!(
        run_prepared(&core, &mut s, &plain, &[]).expect("falls back to public"),
        vec![1]
    );
}

/// Issue #3197: どの要素にも作れない path の作成系 DDL は、拡張クエリでも簡易クエリと同じ SQLSTATE になる
/// （作成先を飛ばして `public` へ黙って作らない）。
#[test]
fn extended_create_with_no_creation_target_matches_the_simple_query() {
    let (core, _g, mut s) = q1_world("sp-ext-create-noschema");
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "SET search_path TO nosuch");
    let simple = code(&core, &mut s, "CREATE TABLE w (id BIGINT PRIMARY KEY)");
    let p = parse_ext(&core, &s, "CREATE TABLE w (id BIGINT PRIMARY KEY)").expect("parse");
    let bound = core.bind_prepared_in_session(&s, &p, &[]).expect("bind");
    let e = core
        .execute_parsed_in_session(&ctx(TENANT), &mut s, &bound)
        .expect_err("must fail like the simple query");
    assert_eq!(e.wire_code(), simple);
}

/// Issue #3197: テナント未束縛の既定セッションでも、path 解決が不要な `COPY`（public 表）は従来どおり Bind できる。
/// 宛先照会を省略するだけで、Execute 直前の宛先照合も固定の陳腐化にならない。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn extended_copy_binds_in_an_unbound_default_session_when_no_path_resolution_is_needed() {
    let (core, _g, mut setup) = q1_world("sp-ext-copy-unbound");
    ok(&core, &mut setup, "CREATE TABLE pt (id BIGINT PRIMARY KEY)");
    let s = SessionState::default();
    assert!(s.resolution_tenant().is_none());
    let copy = parse_ext(&core, &s, "COPY pt TO STDOUT").expect("extended COPY parses");
    let bound = core
        .bind_prepared_in_session(&s, &copy, &[])
        .expect("unbound bind keeps the legacy contract");
    bound.require_session_current(&s).expect("same epoch");
    let txn = core.new_session_transaction();
    core.require_copy_target_current(&s, &txn, &bound)
        .expect("target check is skipped without path resolution");
}

/// Issue #3197: テナント未束縛で path 解決が必要な `COPY` の Bind は、internal ではなく Execute 側と同じ
/// `0A000` で拒否する。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn extended_copy_bind_without_tenant_rejects_with_0a000_when_path_resolution_is_needed() {
    let (core, _g, mut setup) = q1_world("sp-ext-copy-unbound-hide");
    ok(&core, &mut setup, "CREATE TABLE pt (id BIGINT PRIMARY KEY)");
    let mut s = SessionState::default();
    let copy = parse_ext(&core, &s, "COPY pt TO STDOUT").expect("extended COPY parses");
    ok(&core, &mut s, "SET search_path TO q1, public");
    let e = core
        .bind_prepared_in_session(&s, &copy, &[])
        .expect_err("unbound bind needing resolution");
    assert_eq!(e.wire_code(), "0A000");
}

/// Issue #3197（PR #3389 レビュー指摘）: 明示 Tx 内の準備済み `COPY TO` の再 Parse は、名前解決と同じく
/// `BEGIN` 時点のスナップショットで宛先を検証する。BEGIN 後に別接続が宛先を DROP しても再 Parse は失敗しない。
#[test]
fn extended_copy_reparse_in_txn_validates_against_the_begin_snapshot() {
    let (core, _g, mut s) = q1_world("sp-ext-copy-snap");
    assert!(s.bind_resolution_tenant(TENANT));
    let c = ctx(TENANT);
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let copy = std::sync::Arc::new(parse_ext(&core, &s, "COPY t TO STDOUT").expect("parse"));
    // BEGIN 後に別接続が q1.t を DROP する（BEGIN 時点のスナップショットには残っている）。
    let mut other = ddl_session();
    assert!(other.bind_resolution_tenant(TENANT));
    ok(&core, &mut other, "DROP TABLE q1.t");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "SET search_path TO q1, public")
        .expect("set in txn");
    let fresh = core
        .refresh_prepared_in_txn(&s, &txn, &copy)
        .expect("re-parse validates against the BEGIN snapshot");
    assert!(!std::sync::Arc::ptr_eq(&fresh, &copy), "must re-resolve");
}
