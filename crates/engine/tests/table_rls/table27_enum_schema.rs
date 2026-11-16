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
//! - 未対応の形（`DROP TYPE ... CASCADE`・要素つき `CREATE SCHEMA` の `CREATE TYPE` 等）は従来どおり fail-closed
//!
//! Issue #3289: SCHEMA 配下の型を列型（`CREATE TABLE`・`ADD COLUMN`・`ALTER COLUMN .. TYPE`・配列要素）と
//! キャストの型名で解決する。未修飾の列型名は `search_path` で引き、永続化する列定義は内部キー（SCHEMA の
//! 改名に追随する）で持つ。

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

/// 未対応の形は従来どおり fail-closed。組み込み SCHEMA・`pg_temp` への作成は意図的な拒否（ADR D4）。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn unsupported_forms_stay_rejected() {
    let (core, _g) = new_core("t27-enum-unsupported");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a')");
    for (sql, expected) in [
        ("DROP TYPE s.mood CASCADE", "42601"),
        // 型名位置の組み込み SCHEMA・`pg_temp` は拒否（`pg_catalog.int4` を黙って組み込み型にしない）。
        (
            "CREATE TABLE t (id BIGINT PRIMARY KEY, m pg_catalog.int4)",
            "0A000",
        ),
        ("SELECT 'a'::pg_temp.mood", "0A000"),
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
    // SCHEMA 配下の表は public の ENUM 型の列を持てない（D7。SCHEMA 配下の型は持てる）。
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
// ---------------------------------------------------------------------------
// Issue #3289: 列型・キャストでの SCHEMA 配下の型の解決
// ---------------------------------------------------------------------------

/// SELECT の全行を行ごとの文字列（セルの Debug 表現）で返す（public 版との結果比較用）。
fn rows(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<String> {
    match ok(core, s, sql) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| format!("{:?}", row.cells))
            .collect(),
        other => panic!("{sql}: expected Query, got {other:?}"),
    }
}

/// 列型・`ADD COLUMN`・`ALTER COLUMN .. TYPE`・配列要素で `s.mood` を使え、ラベル検証・比較・並びが
/// public の型の同じ表と一致する。
#[test]
fn schema_type_is_usable_as_column_type_with_public_parity() {
    let (core, _g) = new_core("t27-enum-col");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('c', 'a', 'b')");
    ok(&core, &mut s, "CREATE TYPE mood AS ENUM ('c', 'a', 'b')");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.mood, ms s.mood[])",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE pt (id BIGINT PRIMARY KEY, m mood, ms mood[])",
    );
    for t in ["s.t", "pt"] {
        for (id, m) in [(1, "b"), (2, "c"), (3, "a")] {
            ok(
                &core,
                &mut s,
                &format!("INSERT INTO {t} (id, m) VALUES ({id}, '{m}')"),
            );
        }
        ok(
            &core,
            &mut s,
            &format!("INSERT INTO {t} (id, m, ms) VALUES (4, 'a', ARRAY['a', 'b'])"),
        );
    }
    // 語彙外ラベルは public の型と同じ分類で拒否される。
    let bad_pub = code(&core, &mut s, "INSERT INTO pt (id, m) VALUES (9, 'zzz')");
    let bad_sch = code(&core, &mut s, "INSERT INTO s.t (id, m) VALUES (9, 'zzz')");
    assert_eq!(bad_pub, bad_sch);
    let bad_arr = code(
        &core,
        &mut s,
        "INSERT INTO s.t (id, ms) VALUES (9, ARRAY['zzz'])",
    );
    assert_eq!(
        bad_arr,
        code(
            &core,
            &mut s,
            "INSERT INTO pt (id, ms) VALUES (9, ARRAY['zzz'])"
        )
    );
    // 並びは宣言順（c < a < b）で、public の型と一致する。
    for q in [
        "SELECT id, m FROM {} ORDER BY m, id",
        "SELECT id FROM {} WHERE m = 'a' ORDER BY id",
    ] {
        assert_eq!(
            rows(&core, &mut s, &q.replace("{}", "s.t")),
            rows(&core, &mut s, &q.replace("{}", "pt")),
            "{q}"
        );
    }
    // `ADD COLUMN`・`ALTER COLUMN .. TYPE`・複数アクション。
    ok(&core, &mut s, "ALTER TABLE s.t ADD COLUMN m2 s.mood");
    ok(
        &core,
        &mut s,
        "ALTER TABLE s.t ADD COLUMN m3 TEXT, ADD COLUMN m4 s.mood[]",
    );
    // `ALTER COLUMN .. TYPE` の型名位置も解決される（TEXT から ENUM への変更は public の型と同じ分類で拒否）。
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE s.t ALTER COLUMN m3 TYPE s.mood"),
        {
            ok(&core, &mut s, "ALTER TABLE pt ADD COLUMN m3 TEXT");
            code(&core, &mut s, "ALTER TABLE pt ALTER COLUMN m3 TYPE mood")
        }
    );
    // `DROP TYPE` は列が残る間は `2BP01`。
    let (c, msg) = err(&core, &mut s, "DROP TYPE s.mood");
    assert_eq!(c, "2BP01");
    assert!(!msg.contains('#'), "{msg}");
}

/// 同名の型が public と SCHEMA にあるとき、未修飾の列型名は `search_path` の順で解決される。
#[test]
fn unqualified_column_type_follows_the_search_path() {
    let (core, _g) = new_core("t27-enum-col-path");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE mood AS ENUM ('pub')");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('sch')");
    // 既定の path（public のみ）は public の型。SCHEMA 配下の表は public の型を持てない（D7）ので public の表で確かめる。
    ok(
        &core,
        &mut s,
        "CREATE TABLE a (id BIGINT PRIMARY KEY, m mood)",
    );
    ok(&core, &mut s, "INSERT INTO a (id, m) VALUES (1, 'pub')");
    assert_ne!(
        code(&core, &mut s, "INSERT INTO a (id, m) VALUES (2, 'sch')"),
        ""
    );
    // path = s, public: 作成先も型も s。
    ok(&core, &mut s, "SET search_path = s, public");
    ok(
        &core,
        &mut s,
        "CREATE TABLE b (id BIGINT PRIMARY KEY, m mood)",
    );
    ok(&core, &mut s, "INSERT INTO s.b (id, m) VALUES (1, 'sch')");
    assert_ne!(
        code(&core, &mut s, "INSERT INTO s.b (id, m) VALUES (2, 'pub')"),
        ""
    );
    // path = public, s: 型は public（作成先も public）。
    ok(&core, &mut s, "SET search_path = public, s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE c (id BIGINT PRIMARY KEY, m mood)",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO public.c (id, m) VALUES (1, 'pub')",
    );
    // s の型だけを持つ名前は、path の後ろの s で見つかる。
    ok(&core, &mut s, "CREATE TYPE s.only_s AS ENUM ('x')");
    ok(&core, &mut s, "SET search_path = s, public");
    ok(&core, &mut s, "ALTER TABLE s.b ADD COLUMN o only_s");
    ok(
        &core,
        &mut s,
        "INSERT INTO s.b (id, m, o) VALUES (3, 'sch', 'x')",
    );
    // `public` が見えない path で、path のどこにも無い未修飾の型名は `42704`（public へ黙って落とさない）。
    ok(&core, &mut s, "SET search_path = s");
    let (c, msg) = err(
        &core,
        &mut s,
        "CREATE TABLE d (id BIGINT PRIMARY KEY, m nosuch)",
    );
    assert_eq!(c, "42704");
    assert!(!msg.contains('#'), "{msg}");
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE s.b ADD COLUMN o2 nosuch"),
        "42704"
    );
    // 組み込み型名は path に依らず使える。
    ok(&core, &mut s, "ALTER TABLE s.b ADD COLUMN n INTEGER");
    // DDL 権限の無いセッションは存在判定より先に `42501`。
    let mut denied = SessionState::default();
    ok_as(&core, TENANT, &mut denied, "SET search_path = s");
    assert_eq!(
        code(
            &core,
            &mut denied,
            "CREATE TABLE d (id BIGINT PRIMARY KEY, m nosuch)"
        ),
        "42501"
    );
}

/// D7: 表と型の所属が異なる依存は両向きとも `42501`。同じ SCHEMA どうしは許可される。CTAS も同じ。
#[test]
fn schema_dependency_rule_applies_to_enum_in_both_directions() {
    let (core, _g) = new_core("t27-enum-d7");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE SCHEMA s2");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a')");
    ok(&core, &mut s, "CREATE TYPE mood AS ENUM ('a')");
    // public の表 -> SCHEMA の型。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE pt (id BIGINT PRIMARY KEY, m s.mood)"
        ),
        "42501"
    );
    // SCHEMA の表 -> public の型。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m public.mood)"
        ),
        "42501"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m mood)"
        ),
        "42501"
    );
    // 同じテナントの別 SCHEMA の型は許可（FK・SEQUENCE と同じ粒度）。
    ok(
        &core,
        &mut s,
        "CREATE TABLE s2.t (id BIGINT PRIMARY KEY, m s.mood)",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.mood)",
    );
    // ALTER の両向き。
    ok(&core, &mut s, "CREATE TABLE pt (id BIGINT PRIMARY KEY)");
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE pt ADD COLUMN m s.mood"),
        "42501"
    );
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE s.t ADD COLUMN m2 mood"),
        "42501"
    );
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE s.t ALTER COLUMN m TYPE mood"),
        "42501"
    );
    ok(&core, &mut s, "ALTER TABLE pt ADD COLUMN m mood");
    // CTAS（型は元の列から引き継がれる）。
    ok(&core, &mut s, "INSERT INTO s.t (id, m) VALUES (1, 'a')");
    ok(&core, &mut s, "CREATE TABLE s.t2 AS SELECT id, m FROM s.t");
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE pt2 AS SELECT id, m FROM s.t"),
        "42501"
    );
}

/// `DROP TYPE` は SCHEMA 配下の表（配列要素を含む）に使われていれば `2BP01`、`DROP SCHEMA .. CASCADE` は
/// 表→型の順に 1 回で落とし、別 SCHEMA の表が使っていれば何も消さず `2BP01`。
#[test]
fn drop_type_and_drop_schema_see_column_dependencies() {
    let (core, _g) = new_core("t27-enum-dep");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE SCHEMA s2");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a')");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, ms s.mood[])",
    );
    assert_eq!(code(&core, &mut s, "DROP TYPE s.mood"), "2BP01");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s2.u (id BIGINT PRIMARY KEY, m s.mood)",
    );
    // 別 SCHEMA の表が使っているため、`s` の CASCADE は何も消さず拒否される。
    let (c, msg) = err(&core, &mut s, "DROP SCHEMA s CASCADE");
    assert_eq!(c, "2BP01");
    assert!(!msg.contains('#'), "{msg}");
    ok(&core, &mut s, "INSERT INTO s.t (id) VALUES (1)");
    ok(&core, &mut s, "DROP TABLE s2.u");
    ok(&core, &mut s, "DROP SCHEMA s CASCADE");
    ok(&core, &mut s, "CREATE SCHEMA s");
    assert_eq!(code(&core, &mut s, "DROP TYPE s.mood"), "42704");
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.t"), "42P01");
}

/// SCHEMA の改名に列定義が追随する。
#[test]
fn renamed_schema_keeps_column_types() {
    let (core, _g) = new_core("t27-enum-col-rename");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a', 'b')");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.mood)",
    );
    ok(&core, &mut s, "INSERT INTO s.t (id, m) VALUES (1, 'a')");
    ok(&core, &mut s, "ALTER SCHEMA s RENAME TO s2");
    ok(&core, &mut s, "INSERT INTO s2.t (id, m) VALUES (2, 'b')");
    assert_ne!(
        code(&core, &mut s, "INSERT INTO s2.t (id, m) VALUES (3, 'z')"),
        ""
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM s2.t ORDER BY m, id").len(),
        2
    );
    let (c, msg) = err(&core, &mut s, "DROP TYPE s2.mood");
    assert_eq!(c, "2BP01");
    assert!(!msg.contains('#'), "{msg}");
}

/// 2 テナント: 他テナントの SCHEMA の型は不在と同じ `3F000`。同名の SCHEMA・型でも別の型に解決される。
#[test]
fn column_types_do_not_cross_tenants() {
    let (core, _g) = new_core("t27-enum-col-tenants");
    let mut alice = ddl_session();
    let mut bob = ddl_session();
    let mut carol = ddl_session();
    let probe = "CREATE TABLE t (id BIGINT PRIMARY KEY, m s.mood)";
    let baseline = err_as(&core, "carol", &mut carol, probe);
    assert_eq!(baseline.0, "3F000");
    ok_as(&core, "alice", &mut alice, "CREATE SCHEMA s");
    ok_as(
        &core,
        "alice",
        &mut alice,
        "CREATE TYPE s.mood AS ENUM ('a')",
    );
    ok_as(&core, "bob", &mut bob, "CREATE SCHEMA s");
    ok_as(&core, "bob", &mut bob, "CREATE TYPE s.mood AS ENUM ('b')");
    let create = "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.mood)";
    ok_as(&core, "alice", &mut alice, create);
    ok_as(&core, "bob", &mut bob, create);
    ok_as(
        &core,
        "alice",
        &mut alice,
        "INSERT INTO s.t (id, m) VALUES (1, 'a')",
    );
    ok_as(
        &core,
        "bob",
        &mut bob,
        "INSERT INTO s.t (id, m) VALUES (1, 'b')",
    );
    assert_ne!(
        err_as(
            &core,
            "alice",
            &mut alice,
            "INSERT INTO s.t (id, m) VALUES (2, 'b')"
        )
        .0,
        ""
    );
    // 他テナントが同名の SCHEMA・型を持っても、carol の応答は基準と完全に一致する。
    assert_eq!(baseline, err_as(&core, "carol", &mut carol, probe));
    // 内部キー形式（他テナントの ID の推測）は書けない。
    for sql in [
        "CREATE TABLE t2 (id BIGINT PRIMARY KEY, m \"#1.mood\")",
        "CREATE TABLE t2 (id BIGINT PRIMARY KEY, m \"#2.mood\")",
    ] {
        let (c, msg) = err_as(&core, "carol", &mut carol, sql);
        assert!(["42601", "42704"].contains(&c.as_str()), "{sql}: {c} {msg}");
        assert!(!msg.contains('#'), "{msg}");
    }
}

/// キャストの型名は public の型と同じ解決規則を通る（解決後は public と同じ静的な `42704`。
/// ENUM ⇄ 文字列の変換そのものは対象外。`docs/design/cast-conversion-rules.md`）。
#[test]
fn cast_type_names_resolve_like_public() {
    let (core, _g) = new_core("t27-enum-cast");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a')");
    ok(&core, &mut s, "CREATE TYPE mood AS ENUM ('a')");
    let public_cast = err(&core, &mut s, "SELECT 'a'::mood").0;
    assert_eq!(public_cast, "42704");
    for sql in [
        "SELECT 'a'::s.mood",
        "SELECT CAST('a' AS s.mood)",
        "SELECT 'a'::public.mood",
    ] {
        let (c, msg) = err(&core, &mut s, sql);
        assert_eq!(c, public_cast, "{sql}");
        assert!(!msg.contains('#'), "{sql}: {msg}");
    }
    // 修飾された組み込み型名は修飾なしと同じ。
    assert_eq!(
        rows(&core, &mut s, "SELECT 1::public.integer"),
        rows(&core, &mut s, "SELECT 1::integer")
    );
    // 存在しない SCHEMA は `3F000`、組み込み SCHEMA・`pg_temp` は `0A000`。
    assert_eq!(code(&core, &mut s, "SELECT 'a'::z.mood"), "3F000");
    assert_eq!(code(&core, &mut s, "SELECT 'a'::pg_catalog.int4"), "0A000");
    assert_eq!(code(&core, &mut s, "SELECT 'a'::pg_temp.mood"), "0A000");
    // DEFAULT・CHECK 内のキャストは public の型と同じ分類。
    for (sch, pubc) in [
        (
            "CREATE TABLE d1 (id BIGINT PRIMARY KEY, m TEXT DEFAULT 'a'::s.mood)",
            "CREATE TABLE d2 (id BIGINT PRIMARY KEY, m TEXT DEFAULT 'a'::mood)",
        ),
        (
            "CREATE TABLE d3 (id BIGINT PRIMARY KEY, m TEXT, CHECK (m = 'a'::s.mood))",
            "CREATE TABLE d4 (id BIGINT PRIMARY KEY, m TEXT, CHECK (m = 'a'::mood))",
        ),
    ] {
        assert_eq!(code(&core, &mut s, sch), code(&core, &mut s, pubc), "{sch}");
    }
}

/// 一時表の ENUM 列は従来どおり `0A000`（型が SCHEMA 配下でも同じ）。
#[test]
fn temp_table_enum_column_stays_unsupported() {
    let (core, _g) = new_core("t27-enum-temp");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a')");
    ok(&core, &mut s, "CREATE TYPE mood AS ENUM ('a')");
    let public_code = code(
        &core,
        &mut s,
        "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, m mood)",
    );
    assert_eq!(public_code, "0A000");
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, m s.mood)"
        ),
        public_code
    );
}

/// 拡張クエリ: 修飾名の列型は Parse・Bind・Execute で通り、未修飾の列型は Bind 時点の path で再解決される。
#[test]
fn extended_query_resolves_column_types() {
    let (core, _g) = new_core("t27-enum-col-ext");
    let c = ctx(TENANT);
    let mut s = ddl_session();
    assert!(s.bind_resolution_tenant(TENANT));
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TYPE ty AS ENUM ('p')");
    ok(&core, &mut s, "CREATE TYPE s.ty AS ENUM ('s')");
    let create = core
        .parse_sql_prepared_with_declared_in_session(
            &s,
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.ty);",
            &[],
        )
        .expect("parse create");
    let bound = core
        .bind_prepared_in_session(&s, &create, &[])
        .expect("bind create");
    core.execute_parsed_in_session(&c, &mut s, &bound)
        .expect("execute create");
    ok(&core, &mut s, "INSERT INTO s.t (id, m) VALUES (1, 's')");
    // 未修飾の列型は Parse 後に path が変わると、Bind が新しい path で解決し直す。
    ok(&core, &mut s, "SET search_path = s, public");
    let alter = core
        .parse_sql_prepared_with_declared_in_session(&s, "ALTER TABLE s.t ADD COLUMN m2 ty", &[])
        .expect("parse alter");
    ok(&core, &mut s, "SET search_path = public, s");
    // path が public 先頭になると型は public の `ty` に解決され、SCHEMA 配下の表との依存は `42501`。
    let rebound = core.bind_prepared_in_session(&s, &alter, &[]);
    match rebound {
        Ok(b) => {
            let e = core
                .execute_parsed_in_session(&c, &mut s, &b)
                .expect_err("public type in a schema table");
            assert_eq!(e.wire_code(), "42501");
        }
        Err(e) => assert!(
            ["42501", "0A000"].contains(&e.wire_code()),
            "{}",
            e.wire_code()
        ),
    }
}
