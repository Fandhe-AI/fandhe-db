//! ENUM 型の SCHEMA 配置（`CREATE TYPE s.t`／`DROP TYPE s.t`。TABLE-27・TABLE-14・Issue #3288）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-27・TABLE-14、`docs/spec/04-behavior/error-format.md`
//! ERR-6。設計記録は `docs/design/schema-tenant-namespace.md`（追記 Issue #3288）。
//!
//! `table27_schema_objects.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - 修飾名の型を作成・削除でき、同名の `public` の型と独立に共存する（`IF EXISTS` を含む）
//! - `DROP SCHEMA` は RESTRICT で型を残さず拒否し、CASCADE は配下の型も落とす
//! - 他テナントの SCHEMA の型の存在を応答から区別できない（`3F000`）
//! - エラー文言に内部キー（`#`）を出さない
//! - 未対応の形（列型・キャストの `s.mood`、`DROP TYPE ... CASCADE`）は従来どおり fail-closed

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
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

fn ok_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()))
}

fn err_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> (String, String) {
    match core.execute_sql_in_session(&ctx(tenant), s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => (e.wire_code().to_string(), e.to_string()),
    }
}

fn ok(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    ok_as(core, TENANT, s, sql)
}

fn err(core: &EngineCore, s: &mut SessionState, sql: &str) -> (String, String) {
    err_as(core, TENANT, s, sql)
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    err(core, s, sql).0
}

/// 型 `s.mood`・`public.mood` が独立して共存し、削除も独立に効く。
#[test]
fn schema_and_public_types_coexist_and_drop_independently() {
    let (core, _g) = new_core("t27-enum-coexist");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    assert!(matches!(
        ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a', 'b');"),
        SqlOutcome::CreateType(_)
    ));
    assert!(matches!(
        ok(&core, &mut s, "CREATE TYPE mood AS ENUM ('x')"),
        SqlOutcome::CreateType(_)
    ));
    // public の型は語彙 `x` だけを持つ（SCHEMA 配下の語彙と混ざらない）。
    ok(
        &core,
        &mut s,
        "CREATE TABLE pt (id BIGINT PRIMARY KEY, m mood)",
    );
    ok(&core, &mut s, "INSERT INTO pt (id, m) VALUES (1, 'x')");
    assert_ne!(
        code(&core, &mut s, "INSERT INTO pt (id, m) VALUES (2, 'a')"),
        ""
    );
    // 重複は `42710`。文言は公開名で、内部キーを含まない。
    let (c, msg) = err(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('z')");
    assert_eq!(c, "42710");
    assert!(msg.contains("s.mood"), "{msg}");
    assert!(!msg.contains('#'), "{msg}");
    // 削除は独立。public の型は使用中なので `2BP01`、SCHEMA 配下の型は消せる。
    assert_eq!(code(&core, &mut s, "DROP TYPE mood"), "2BP01");
    ok(&core, &mut s, "DROP TYPE s.mood");
    let (c, msg) = err(&core, &mut s, "DROP TYPE s.mood");
    assert_eq!(c, "42704");
    assert!(!msg.contains('#'), "{msg}");
    assert!(matches!(
        ok(&core, &mut s, "DROP TYPE IF EXISTS s.mood"),
        SqlOutcome::DropType(_)
    ));
    // SCHEMA が在って型が無い `IF EXISTS` は no-op 成功。public の型は無傷。
    ok(&core, &mut s, "INSERT INTO pt (id, m) VALUES (3, 'x')");
    ok(&core, &mut s, "DROP TABLE pt");
    ok(&core, &mut s, "DROP TYPE mood");
}

/// `public.` 明示修飾は `search_path` の先頭が SCHEMA でも（`public` が隠れていても）public へ作る。
#[test]
fn public_qualified_create_ignores_the_path() {
    let (core, _g) = new_core("t27-enum-public-qualified");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE public.m2 AS ENUM ('a')");
    ok(&core, &mut s, "SET search_path = s");
    ok(&core, &mut s, "CREATE TYPE public.m3 AS ENUM ('a')");
    ok(&core, &mut s, "DROP TYPE public.m3");
    ok(&core, &mut s, "SET search_path = DEFAULT");
    ok(&core, &mut s, "DROP TYPE m2");
}

/// 未修飾の `CREATE TYPE`／`DROP TYPE` は `search_path` で解決する。同名の public の型があっても、未修飾の
/// `DROP TYPE` は path 先頭の SCHEMA の型を落とし、共有の public の型を取り違えて落とさない。
#[test]
fn unqualified_type_ddl_follows_the_search_path() {
    let (core, _g) = new_core("t27-enum-path");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE ty AS ENUM ('p')");
    ok(&core, &mut s, "SET search_path = s, public");
    ok(&core, &mut s, "CREATE TYPE ty2 AS ENUM ('a')");
    // `ty2` は s の中にあり、public には無い。
    assert_eq!(code(&core, &mut s, "DROP TYPE public.ty2"), "42704");
    // 同名の型が s と public の両方にあるとき、未修飾は s 側。
    ok(&core, &mut s, "CREATE TYPE s.ty AS ENUM ('s')");
    ok(&core, &mut s, "DROP TYPE ty");
    assert_eq!(code(&core, &mut s, "DROP TYPE s.ty"), "42704");
    // s の `ty` が無くなれば、次の未修飾 `DROP TYPE ty` は path の次の public を落とす。
    ok(&core, &mut s, "DROP TYPE ty");
    assert_eq!(code(&core, &mut s, "DROP TYPE public.ty"), "42704");
    // 表だけが同名で在る SCHEMA は型として解決されない（名前空間が別）。
    ok(&core, &mut s, "CREATE TYPE public.ty3 AS ENUM ('p')");
    ok(&core, &mut s, "CREATE TABLE ty3 (id BIGINT PRIMARY KEY)");
    ok(&core, &mut s, "DROP TYPE ty3");
    assert_eq!(code(&core, &mut s, "DROP TYPE public.ty3"), "42704");
    ok(&core, &mut s, "DROP TABLE s.ty3");
    ok(&core, &mut s, "DROP TYPE s.ty2");
}

/// RESTRICT は型だけを持つ SCHEMA を拒否し、CASCADE は型を落とす。作り直した SCHEMA に古い型は見えない。
#[test]
fn drop_schema_handles_types() {
    let (core, _g) = new_core("t27-enum-drop-schema");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a')");
    ok(&core, &mut s, "CREATE TYPE mood AS ENUM ('p')");
    let (c, msg) = err(&core, &mut s, "DROP SCHEMA s");
    assert_eq!(c, "2BP01");
    assert!(!msg.contains('#'), "{msg}");
    ok(&core, &mut s, "DROP SCHEMA s CASCADE");
    ok(&core, &mut s, "CREATE SCHEMA s");
    assert_eq!(code(&core, &mut s, "DROP TYPE s.mood"), "42704");
    // 作り直した SCHEMA で同名の型を作れる。public の型は無傷。
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('b')");
    ok(&core, &mut s, "DROP TYPE mood");
    // 権限の無い主体は CASCADE を実行できない。
    let mut plain = SessionState::default();
    assert_eq!(code(&core, &mut plain, "DROP SCHEMA s CASCADE"), "42501");
    ok(&core, &mut s, "DROP SCHEMA s CASCADE");
}

/// `ALTER SCHEMA .. RENAME` に型が追随する。
#[test]
fn renamed_schema_keeps_its_types() {
    let (core, _g) = new_core("t27-enum-rename");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a')");
    ok(&core, &mut s, "ALTER SCHEMA s RENAME TO s2");
    assert_eq!(code(&core, &mut s, "DROP TYPE s.mood"), "3F000");
    ok(&core, &mut s, "DROP TYPE s2.mood");
}

/// 同名の SCHEMA・型を持つ 2 テナントが互いに影響せず、SCHEMA を持たないテナントは他テナントの SCHEMA を
/// 存在しない SCHEMA と区別できない。
#[test]
fn tenants_do_not_see_each_others_schema_types() {
    let (core, _g) = new_core("t27-enum-tenants");
    let mut carol = ddl_session();
    let probes = [
        "CREATE TYPE s.mood AS ENUM ('a')",
        "DROP TYPE s.mood",
        "DROP TYPE IF EXISTS s.mood",
    ];
    // SCHEMA `s` がどのテナントにも無い間の応答を基準にする。
    let baseline: Vec<(String, String)> = probes
        .iter()
        .map(|sql| err_as(&core, "carol", &mut carol, sql))
        .collect();
    assert!(baseline.iter().all(|(c, _)| c == "3F000"), "{baseline:?}");

    let mut alice = ddl_session();
    let mut bob = ddl_session();
    for (t, s) in [("alice", &mut alice), ("bob", &mut bob)] {
        ok_as(&core, t, s, "CREATE SCHEMA s");
        ok_as(&core, t, s, "CREATE TYPE s.mood AS ENUM ('a')");
    }
    // 他テナントが同名の SCHEMA・型を持っても、carol の応答は基準と完全に一致する。
    let after: Vec<(String, String)> = probes
        .iter()
        .map(|sql| err_as(&core, "carol", &mut carol, sql))
        .collect();
    assert_eq!(baseline, after);
    // bob の削除は alice の型に影響しない。
    ok_as(&core, "bob", &mut bob, "DROP TYPE s.mood");
    assert_eq!(
        err_as(
            &core,
            "alice",
            &mut alice,
            "CREATE TYPE s.mood AS ENUM ('z')"
        )
        .0,
        "42710"
    );
    ok_as(&core, "bob", &mut bob, "DROP SCHEMA s CASCADE");
    assert_eq!(
        err_as(
            &core,
            "alice",
            &mut alice,
            "CREATE TYPE s.mood AS ENUM ('z')"
        )
        .0,
        "42710"
    );
    ok_as(&core, "alice", &mut alice, "DROP TYPE s.mood");
}

/// 解析段の修飾名の解決は、権限の有無で応答が分かれない（`CREATE SEQUENCE`・`DROP SEQUENCE` と同じ判定順）。
/// 既存の SCHEMA でも、権限の無いセッションは `42501`。
#[test]
fn name_resolution_order_matches_sequences() {
    let (core, _g) = new_core("t27-enum-order");
    let mut allowed = ddl_session();
    let mut denied = SessionState::default();
    ok(&core, &mut allowed, "CREATE SCHEMA s");
    ok(&core, &mut allowed, "CREATE TYPE s.mood AS ENUM ('a')");
    for (ty, seq) in [
        (
            "CREATE TYPE nosuch.t1 AS ENUM ('a')",
            "CREATE SEQUENCE nosuch.q",
        ),
        ("DROP TYPE nosuch.t1", "DROP SEQUENCE nosuch.q"),
        (
            "DROP TYPE IF EXISTS nosuch.t1",
            "DROP SEQUENCE IF EXISTS nosuch.q",
        ),
    ] {
        for s in [&mut allowed, &mut denied] {
            assert_eq!(code(&core, s, ty), code(&core, s, seq), "{ty}");
        }
    }
    for sql in ["CREATE TYPE s.x AS ENUM ('a')", "DROP TYPE s.mood"] {
        assert_eq!(code(&core, &mut denied, sql), "42501", "{sql}");
    }
    // 権限の無い拒否はカタログを変えない。
    assert_eq!(
        code(&core, &mut allowed, "CREATE TYPE s.mood AS ENUM ('z')"),
        "42710"
    );
}

/// 未対応の形は従来どおり fail-closed。列型・キャストは Issue #3289。組み込み SCHEMA・`pg_temp` への作成は
/// 意図的な拒否（ADR D4）。
// intentional: docs/design/schema-tenant-namespace.md
// tracked: #3289
#[test]
fn unsupported_forms_stay_rejected() {
    let (core, _g) = new_core("t27-enum-unsupported");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a')");
    for (sql, expected) in [
        ("CREATE TABLE t (id BIGINT PRIMARY KEY, m s.mood)", "42601"),
        ("SELECT 'a'::s.mood", "42601"),
        ("DROP TYPE s.mood CASCADE", "42601"),
        ("CREATE TYPE pg_catalog.x AS ENUM ('a')", "0A000"),
        ("CREATE TYPE pg_temp.x AS ENUM ('a')", "0A000"),
        ("DROP TYPE pg_catalog.x", "0A000"),
        // 要素つき `CREATE SCHEMA` の `CREATE TYPE` 要素は従来どおり `0A000`（Issue #3286 の D2）。
        ("CREATE SCHEMA s3 CREATE TYPE mood AS ENUM ('a')", "0A000"),
    ] {
        assert_eq!(code(&core, &mut s, sql), expected, "{sql}");
    }
    // 拒否はカタログを変えない（型は残っている）。
    assert_eq!(
        code(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('z')"),
        "42710"
    );
    // SCHEMA 配下の表は ENUM 列を持てない（D7）。
    ok(&core, &mut s, "CREATE TYPE mood AS ENUM ('p')");
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m mood)"
        ),
        "42501"
    );
}

/// 上限は SCHEMA 配下でも維持され、予約名（組み込み型名）は名前部で拒否される。
#[test]
fn limits_and_reserved_names_apply_inside_a_schema() {
    let (core, _g) = new_core("t27-enum-limits");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    let labels: Vec<String> = (0..257).map(|i| format!("'l{i}'")).collect();
    let sql = format!("CREATE TYPE s.big AS ENUM ({})", labels.join(", "));
    assert_eq!(code(&core, &mut s, &sql), "54000");
    let public_code = code(&core, &mut s, "CREATE TYPE int4 AS ENUM ('a')");
    assert_eq!(
        code(&core, &mut s, "CREATE TYPE s.int4 AS ENUM ('a')"),
        public_code
    );
}

/// 拡張クエリ: Parse した未修飾の `DROP TYPE` は Bind 時点の path で再解決される。
#[test]
fn extended_query_re_resolves_unqualified_drop_type() {
    let (core, _g) = new_core("t27-enum-extended");
    let c = ctx(TENANT);
    let mut s = ddl_session();
    // 拡張クエリの Parse が SCHEMA 修飾・path を解決するテナント（wire の handshake 相当）。
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE ty AS ENUM ('p')");
    ok(&core, &mut s, "CREATE TYPE s.ty AS ENUM ('s')");
    ok(&core, &mut s, "SET search_path = s, public");
    let drop_ty = core
        .parse_sql_prepared_with_declared_in_session(&s, "DROP TYPE ty", &[])
        .expect("parse");
    // Parse 後に path が変わると、Bind は新しい path（public のみ）で解決し直す。
    ok(&core, &mut s, "SET search_path = public");
    let bound = core
        .bind_prepared_in_session(&s, &drop_ty, &[])
        .expect("bind");
    core.execute_parsed_in_session(&c, &mut s, &bound)
        .expect("execute");
    // public の `ty` が落ち、s の `ty` は残る。
    assert_eq!(code(&core, &mut s, "DROP TYPE public.ty"), "42704");
    ok(&core, &mut s, "DROP TYPE s.ty");
    // 修飾名の CREATE TYPE の Parse・Bind・Execute。
    let create = core
        .parse_sql_prepared_with_declared_in_session(&s, "CREATE TYPE s.ty2 AS ENUM ('a');", &[])
        .expect("parse create");
    let bound = core
        .bind_prepared_in_session(&s, &create, &[])
        .expect("bind create");
    core.execute_parsed_in_session(&c, &mut s, &bound)
        .expect("execute create");
    ok(&core, &mut s, "DROP TYPE s.ty2");
}

/// 内部キー形式（`#<id>.<name>`）の型名を利用者が直接書いても拒否される（内部キーを作れるのは解決器だけ）。
/// 他テナントの SCHEMA の型を指す内部キーを推測して書いても、その型は作成・削除できず、存在も観測できない。
#[test]
fn internal_keys_cannot_be_written_by_users() {
    let (core, _g) = new_core("t27-enum-raw-keys");
    let mut alice = ddl_session();
    let mut bob = ddl_session();
    ok_as(&core, "alice", &mut alice, "CREATE SCHEMA s");
    ok_as(
        &core,
        "alice",
        &mut alice,
        "CREATE TYPE s.mood AS ENUM ('a')",
    );
    ok_as(&core, "bob", &mut bob, "CREATE SCHEMA own");
    // 内部 ID は 1 始まりで再利用されない。alice の SCHEMA は 1、bob の SCHEMA は 2 のはず。
    for sql in [
        "CREATE TYPE \"#1.mood\" AS ENUM ('z')",
        "CREATE TYPE \"#2.mood\" AS ENUM ('z')",
        "DROP TYPE \"#1.mood\"",
        "DROP TYPE IF EXISTS \"#1.mood\"",
        "DROP TYPE \"#2.mood\"",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, m \"#1.mood\")",
    ] {
        for (tenant, s) in [("alice", &mut alice), ("bob", &mut bob)] {
            let (c, msg) = err_as(&core, tenant, s, sql);
            assert!(["42601", "42704"].contains(&c.as_str()), "{sql}: {c} {msg}");
            assert!(!msg.contains('#'), "{sql}: {msg}");
        }
    }
    // alice の型は無傷（作成すると重複、bob 側からは SCHEMA が無いので `3F000`）。
    assert_eq!(
        err_as(
            &core,
            "alice",
            &mut alice,
            "CREATE TYPE s.mood AS ENUM ('z')"
        )
        .0,
        "42710"
    );
    assert_eq!(
        err_as(&core, "bob", &mut bob, "DROP TYPE s.mood").0,
        "3F000"
    );
}
