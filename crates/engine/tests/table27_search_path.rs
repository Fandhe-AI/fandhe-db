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

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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

    // 自テナントの既存 SCHEMA が先頭なら、テナント SCHEMA への作成は #1717 まで `0A000`。
    ok(&core, &mut s, "RESET search_path");
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "SET search_path = s, public");
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE w (id BIGINT PRIMARY KEY)"),
        "0A000"
    );
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE sq2"), "0A000");
    // 読み取りは public の表を引ける（空の SCHEMA は飛ばされる）。
    ok(&core, &mut s, "SELECT id FROM t LIMIT 5");
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
    // username と同名の SCHEMA が既定の path で先に見つかる（#1717 まで `0A000`）。
    s.set_session_user("alice");
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE u1 (id BIGINT PRIMARY KEY)"),
        "0A000"
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
