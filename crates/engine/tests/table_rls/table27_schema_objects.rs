//! テナント SCHEMA 配下のオブジェクト配置（TABLE-27・Issue #1717）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-27、`docs/spec/04-behavior/error-format.md`
//! ERR-6。設計記録は `docs/design/schema-tenant-namespace.md`（D2・D5・D7・D8。実装記録は §8）。
//!
//! `table27_qualified_names.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - 表・ビュー・索引・SEQUENCE・UDF を `s.name` の修飾名で作成・参照・変更・削除できる
//! - 同名の public オブジェクトとは独立している（行・索引・採番値が混ざらない）
//! - 他テナントの SCHEMA は存在しないものと同じ応答（`3F000`）
//! - エラー文言・EXPLAIN に内部キー（`#`・内部 ID）を出さない
//! - 利用者が内部キーを直接書いても拒否される

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

fn run_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx(tenant), s, sql) {
        Ok(o) => format!("{o:?}"),
        Err(e) => format!("ERR {}", e.wire_code()),
    }
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    run_as(core, TENANT, s, sql)
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

fn count(core: &EngineCore, s: &mut SessionState, table: &str) -> String {
    match ok(core, s, &format!("SELECT COUNT(*) FROM {table}")) {
        SqlOutcome::Query(r) => format!("{:?}", r.rows[0].cells[0]),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// `{Q}` を修飾子（空文字または `s.`）へ置換して実行する、修飾の有無で結果が一致するべき台本
/// （`table27_qualified_names.rs` の台本と同じ形。ビュー・SEQUENCE も同じ SCHEMA に置く）。
const SCRIPT: &[&str] = &[
    "CREATE TABLE {Q}t (id BIGINT PRIMARY KEY, name TEXT NOT NULL, qty INTEGER DEFAULT 7, code TEXT UNIQUE)",
    "CREATE TABLE IF NOT EXISTS {Q}t (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
    "CREATE TABLE {Q}p (id BIGINT PRIMARY KEY, name TEXT)",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (1, 'a', 10, 'c1')",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (2, 'b', 20, 'c2'), (3, 'c', 30, 'c3')",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (9, 'x', 1, 'c1')",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (1, 'a2', 11, 'c1') ON CONFLICT (id) DO UPDATE SET name = 'a2'",
    "INSERT INTO {Q}t (id, name, qty, code) VALUES (5, 'e', 50, 'c5') RETURNING id, name",
    "INSERT INTO {Q}p (id, name) VALUES (1, 'a2'), (2, 'b')",
    "SELECT id, name, qty FROM {Q}t ORDER BY id LIMIT 10",
    "SELECT t.id, t.name FROM {Q}t WHERE t.qty > 15 ORDER BY t.id LIMIT 10",
    "SELECT x.id, x.name FROM {Q}t AS x WHERE x.qty > 15 ORDER BY x.id LIMIT 10",
    "SELECT COUNT(*), SUM(qty) FROM {Q}t",
    "SELECT a.id, b.name FROM {Q}t a JOIN {Q}p b ON a.name = b.name LIMIT 10",
    "SELECT id FROM {Q}t WHERE name IN (SELECT name FROM {Q}p LIMIT 10) ORDER BY id LIMIT 10",
    "SELECT id FROM {Q}t WHERE EXISTS (SELECT id FROM {Q}p LIMIT 10) ORDER BY id LIMIT 10",
    "WITH c AS (SELECT id, name FROM {Q}t LIMIT 10) SELECT id FROM c LIMIT 10",
    "SELECT id FROM {Q}t UNION SELECT id FROM {Q}p",
    "UPDATE {Q}t SET qty = 51 WHERE id = 5 RETURNING id, qty",
    "DELETE FROM {Q}t WHERE id = 5 RETURNING id, name",
    "CREATE INDEX t_qty ON {Q}t (qty)",
    "CREATE TABLE {Q}c (id BIGINT PRIMARY KEY, pid BIGINT REFERENCES {Q}p (id))",
    "INSERT INTO {Q}c (id, pid) VALUES (1, 1)",
    "INSERT INTO {Q}c (id, pid) VALUES (2, 99)",
    "ALTER TABLE {Q}t ADD COLUMN extra TEXT",
    "SELECT id, extra FROM {Q}t ORDER BY id LIMIT 10",
    "CREATE VIEW {Q}v AS SELECT id, name FROM {Q}t",
    "SELECT id, name FROM {Q}v ORDER BY id LIMIT 10",
    "DROP VIEW {Q}v",
    "TRUNCATE {Q}c, {Q}p",
    "SELECT COUNT(*) FROM {Q}p",
    "DROP INDEX {Q}t_qty",
    "DROP TABLE IF EXISTS {Q}missing",
    "DROP TABLE {Q}c",
    "DROP TABLE {Q}p",
    "DROP TABLE {Q}t",
    "SELECT id FROM {Q}t LIMIT 1",
];

fn script(qualifier: &str) -> Vec<String> {
    let (core, _g) = new_core("t27-objects-script");
    let mut s = ddl_session();
    if !qualifier.is_empty() {
        ok(&core, &mut s, "CREATE SCHEMA s");
    }
    SCRIPT
        .iter()
        .map(|q| run(&core, &mut s, &q.replace("{Q}", qualifier)))
        // Rust の結果型（Debug）に出る内部 ID は比較から除く（利用者向けの文言は別テストで検証する）。
        .map(|r| r.replace("#1.", ""))
        .collect()
}

/// 表・索引・ビューの作成・参照・変更・削除が、修飾の有無で同じ結果になる。
#[test]
fn tenant_schema_objects_behave_like_unqualified_objects() {
    let plain = script("");
    let qualified = script("s.");
    assert_eq!(plain.len(), qualified.len());
    let diffs: Vec<String> = plain
        .iter()
        .zip(&qualified)
        .enumerate()
        .filter(|(_, (p, q))| p != q)
        .map(|(i, (p, q))| format!("step {i}: {}\n  plain: {p}\n  qual : {q}", SCRIPT[i]))
        .collect();
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
    let errors = plain.iter().filter(|r| r.starts_with("ERR")).count();
    assert!(errors >= 3 && errors < plain.len() - 10, "{plain:#?}");
}

/// 広い文種の台本（ALTER・TRUNCATE・制約・UPSERT・集計・結合・サブクエリ・明示トランザクション・分割実行 DML）。
/// SCHEMA 配下の表で実行した結果が、public の同名の表と一致する（内部 ID を除く）。
const WIDE_SCRIPT: &[&str] = &[
    "CREATE TABLE {Q}p (id BIGINT PRIMARY KEY, code TEXT UNIQUE, label TEXT)",
    "CREATE TABLE {Q}t (id BIGINT PRIMARY KEY, name TEXT NOT NULL, qty INTEGER DEFAULT 7, pc TEXT REFERENCES {Q}p (code), CONSTRAINT t_qty_check CHECK (qty >= 0))",
    "INSERT INTO {Q}p (id, code, label) VALUES (1, 'a', 'A'), (2, 'b', 'B')",
    "INSERT INTO {Q}t (id, name, qty, pc) VALUES (1, 'x', 1, 'a'), (2, 'y', 2, 'b'), (3, 'z', 3, 'a')",
    "INSERT INTO {Q}t (id, name, qty, pc) VALUES (4, 'w', -1, 'a')",
    "INSERT INTO {Q}t (id, name, qty, pc) VALUES (5, 'w', 1, 'zzz')",
    "UPDATE {Q}t SET qty = qty + 1 WHERE name = 'x'",
    "DELETE FROM {Q}p WHERE code = 'b'",
    "SELECT name, qty FROM {Q}t WHERE qty > 1 ORDER BY id LIMIT 10",
    "SELECT pc, COUNT(*) AS n, SUM(qty) AS s FROM {Q}t GROUP BY pc ORDER BY pc LIMIT 10",
    "SELECT t.name, p.label FROM {Q}t JOIN {Q}p p ON t.pc = p.code ORDER BY t.id LIMIT 10",
    "SELECT name FROM {Q}t WHERE pc IN (SELECT code FROM {Q}p WHERE label = 'A' LIMIT 5) ORDER BY id LIMIT 10",
    "INSERT INTO {Q}t (id, name, qty, pc) VALUES (1, 'x2', 9, 'a') ON CONFLICT (id) DO UPDATE SET name = 'x2'",
    "ALTER TABLE {Q}t ADD COLUMN extra TEXT DEFAULT 'e'",
    "ALTER TABLE {Q}t ADD CONSTRAINT t_name_key UNIQUE (name)",
    "ALTER TABLE {Q}t ADD CONSTRAINT t_extra_check CHECK (extra <> 'bad')",
    "ALTER TABLE {Q}t RENAME COLUMN extra TO note",
    "ALTER TABLE {Q}t ALTER COLUMN note SET DEFAULT 'n'",
    "ALTER TABLE {Q}t ALTER COLUMN note DROP DEFAULT",
    "ALTER TABLE {Q}t DROP CONSTRAINT t_name_key",
    "ALTER TABLE {Q}t ALTER COLUMN qty TYPE BIGINT",
    "ALTER TABLE {Q}t DROP COLUMN note",
    "SELECT id, name, qty FROM {Q}t ORDER BY id LIMIT 10",
    "BEGIN",
    "INSERT INTO {Q}t (id, name, qty, pc) VALUES (10, 'tx', 1, 'a')",
    "SELECT COUNT(*) FROM {Q}t",
    "ROLLBACK",
    "SELECT COUNT(*) FROM {Q}t",
    "BEGIN",
    "UPDATE {Q}t SET qty = 5 WHERE id = 2",
    "COMMIT",
    "UPDATE {Q}t SET qty = 6 WHERE qty > 100 PARTITIONED USING OPERATION_ID 'pu1'",
    "EXPLAIN SELECT id FROM {Q}t WHERE id = 1 LIMIT 5",
    "TRUNCATE {Q}t",
    "SELECT COUNT(*) FROM {Q}t",
    "DROP TABLE {Q}p",
    "DROP TABLE {Q}t",
    "DROP TABLE {Q}p CASCADE",
];

fn wide_script(qualifier: &str) -> Vec<String> {
    let (core, _g) = new_core("t27-objects-wide");
    let mut s = ddl_session();
    if !qualifier.is_empty() {
        ok(&core, &mut s, "CREATE SCHEMA s");
    }
    // 明示トランザクションを使うため `execute_sql_in_txn` で実行する。
    let mut txn = core.new_session_transaction();
    WIDE_SCRIPT
        .iter()
        .map(|q| {
            let sql = q.replace("{Q}", qualifier);
            match core.execute_sql_in_txn(&ctx(TENANT), &mut s, &mut txn, &sql) {
                Ok(o) => format!("{o:?}"),
                Err(e) => format!("ERR {}", e.wire_code()),
            }
        })
        .map(|r| r.replace("#1.", ""))
        .collect()
}

#[test]
fn wide_statement_kinds_behave_like_public() {
    let plain = wide_script("");
    let qualified = wide_script("s.");
    assert_eq!(plain.len(), qualified.len());
    let diffs: Vec<String> = plain
        .iter()
        .zip(&qualified)
        .enumerate()
        .filter(|(_, (p, q))| p != q)
        .map(|(i, (p, q))| {
            format!(
                "step {i}: {}\n  plain: {}\n  qual : {}",
                WIDE_SCRIPT[i],
                &p[..p.len().min(300)],
                &q[..q.len().min(300)]
            )
        })
        .collect();
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
    // 台本が空振りしていない（成功する文が十分ある）。
    let oks = plain.iter().filter(|r| !r.starts_with("ERR")).count();
    assert!(oks >= 25, "{plain:#?}");
}

/// COPY（`FROM STDIN`・`(SELECT ..) TO STDOUT`）も修飾名を解決する。別テナントは `3F000`。
#[test]
fn copy_resolves_schema_qualified_tables() {
    let (core, _g) = new_core("t27-objects-copy");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA a");
    ok(
        &core,
        &mut s,
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO a.t (id, name) VALUES (1, 'x')");
    let plan = core
        .begin_copy(&ctx(TENANT), &s, "COPY a.t (id, name) FROM STDIN")
        .expect("copy from");
    assert!(matches!(plan, engine::core::CopyPlan::From(..)));
    let plan = core
        .begin_copy(
            &ctx(TENANT),
            &s,
            "COPY (SELECT id, name FROM a.t LIMIT 10) TO STDOUT",
        )
        .expect("copy to");
    assert!(matches!(plan, engine::core::CopyPlan::To(..)));
    for sql in [
        "COPY a.t (id, name) FROM STDIN",
        "COPY (SELECT id, name FROM a.t LIMIT 10) TO STDOUT",
    ] {
        let e = core
            .begin_copy(&ctx("bob"), &s, sql)
            .map(|_| ())
            .expect_err(sql);
        assert_eq!(e.wire_code(), "3F000", "{sql}");
    }
}

/// 拡張クエリ: 接続に束縛したテナントで Parse が SCHEMA 修飾を解決し、Parse の後に SCHEMA 配下の表が
/// 消えた場合の Bind／実行のエラーにも内部キーが出ない。束縛したテナントと異なる ctx での実行は拒否する。
#[test]
fn prepared_statements_resolve_with_the_bound_tenant_and_hide_internal_keys() {
    let (core, _g) = new_core("t27-objects-prepared");
    let mut s = ddl_session();
    assert!(s.bind_resolution_tenant(TENANT));
    // 別のテナントへの付け替えはできない。
    assert!(!s.bind_resolution_tenant("bob"));
    assert_eq!(s.resolution_tenant(), Some(TENANT));
    ok(&core, &mut s, "CREATE SCHEMA a");
    ok(
        &core,
        &mut s,
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO a.t (id, name) VALUES (1, 'x')");
    let prepared = core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT id FROM a.t LIMIT 5", &[])
        .expect("parse");
    // Parse と Execute の間に表が消える。
    ok(&core, &mut s, "DROP TABLE a.t");
    let bound = core.bind_prepared(&prepared, &[]);
    let err = match bound {
        Err(e) => e,
        Ok(parsed) => core
            .execute_parsed_in_session(&ctx(TENANT), &mut s, &parsed)
            .map(|_| ())
            .expect_err("table is gone"),
    };
    assert_eq!(err.wire_code(), "42P01");
    assert!(!err.to_string().contains('#'), "{err}");
    // Parse 済みの未束縛テナントでの SCHEMA 修飾は 3F000（fail-closed）。
    let unbound = SessionState::default();
    let e = core
        .parse_sql_prepared_with_declared_in_session(&unbound, "SELECT id FROM a.t LIMIT 5", &[])
        .map(|_| ())
        .expect_err("unbound");
    assert_eq!(e.wire_code(), "3F000");
    // 束縛したテナントと異なる ctx での実行は拒否される。
    let e = core
        .execute_sql_in_session(&ctx("bob"), &mut s, "SELECT 1")
        .map(|_| ())
        .expect_err("tenant mismatch");
    assert_eq!(e.wire_code(), "XX000");
}

/// 永続化の往復: 再オープン後も SCHEMA 配下の表・ビュー・索引・SEQUENCE・DEFAULT が使える
/// （decode 側の relation キー検証と、格納形式の版を上げないことの確認）。
#[test]
fn schema_objects_survive_reopen() {
    let path = unique_db_path("t27-objects-reopen");
    let _guard = CleanupGuard(path.clone());
    let open = |p: &std::path::Path| {
        EngineCore::from_storage(
            Storage::open(p).expect("open storage"),
            Box::new(CpuScalarProvider),
        )
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
    };
    let mut s = ddl_session();
    {
        let core = open(&path);
        ok(&core, &mut s, "CREATE SCHEMA a");
        ok(&core, &mut s, "CREATE SEQUENCE a.q START 10");
        ok(&core, &mut s, "CREATE TABLE a.t (id BIGSERIAL PRIMARY KEY, name TEXT UNIQUE, n BIGINT DEFAULT nextval('a.q'))");
        ok(&core, &mut s, "CREATE INDEX i ON a.t (name)");
        ok(&core, &mut s, "CREATE VIEW a.v AS SELECT id, name FROM a.t");
        ok(&core, &mut s, "INSERT INTO a.t (name) VALUES ('x')");
    }
    let core = open(&path);
    let mut s = ddl_session();
    assert_eq!(
        query_rows(
            &core,
            &mut s,
            "SELECT id, name FROM a.v ORDER BY id LIMIT 5"
        ),
        ["[SignedInteger(1), Text(\"x\")]"]
    );
    ok(&core, &mut s, "INSERT INTO a.t (name) VALUES ('y')");
    assert_eq!(
        query_rows(
            &core,
            &mut s,
            "SELECT id, n FROM a.t WHERE name = 'y' LIMIT 5"
        ),
        ["[SignedInteger(2), SignedInteger(11)]"]
    );
    assert_eq!(
        code(&core, &mut s, "INSERT INTO a.t (name) VALUES ('y')"),
        "23505"
    );
    assert_eq!(code(&core, &mut s, "CREATE INDEX i ON a.t (name)"), "42P07");
    assert_eq!(code(&core, &mut s, "DROP SCHEMA a"), "2BP01");
    ok(&core, &mut s, "DROP SCHEMA a CASCADE");
    // 作り直した SCHEMA は空（内部 ID は再オープンをまたいで再利用されない）。
    ok(&core, &mut s, "CREATE SCHEMA a");
    assert_eq!(code(&core, &mut s, "SELECT id FROM a.t LIMIT 1"), "42P01");
}

/// 一時表と同名の永続表を SCHEMA 修飾で参照しても、一時表へ化けない（修飾名は永続の SCHEMA 配下を指す）。
#[test]
fn schema_qualified_name_is_not_shadowed_by_a_same_name_temp_table() {
    let (core, _g) = new_core("t27-objects-temp");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA a");
    ok(
        &core,
        &mut s,
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO a.t (id, name) VALUES (1, 'perm')",
    );
    ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO t (id, name) VALUES (7, 'temp')");
    assert_eq!(
        query_rows(&core, &mut s, "SELECT id, name FROM a.t LIMIT 5"),
        ["[SignedInteger(1), Text(\"perm\")]"]
    );
    assert_eq!(
        query_rows(&core, &mut s, "SELECT id, name FROM t LIMIT 5"),
        ["[SignedInteger(7), Text(\"temp\")]"]
    );
}

/// `ALTER COLUMN SET DEFAULT nextval(..)` は作成時に SEQUENCE を解決する（早期束縛）。public の表から
/// SCHEMA 配下の SEQUENCE への設定は D7 で `42501`。
#[test]
fn set_default_nextval_binds_the_sequence_early() {
    let (core, _g) = new_core("t27-objects-setdefault");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE SEQUENCE s.q START 50");
    ok(&core, &mut s, "CREATE SEQUENCE q START 5");
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT)",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, n BIGINT)",
    );
    ok(
        &core,
        &mut s,
        "ALTER TABLE t ALTER COLUMN n SET DEFAULT nextval('q')",
    );
    ok(
        &core,
        &mut s,
        "ALTER TABLE s.t ALTER COLUMN n SET DEFAULT nextval('s.q')",
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "ALTER TABLE t ALTER COLUMN n SET DEFAULT nextval('s.q')"
        ),
        "42501"
    );
    ok(&core, &mut s, "INSERT INTO t (id) VALUES (1)");
    ok(&core, &mut s, "INSERT INTO s.t (id) VALUES (1)");
    assert_eq!(
        query_rows(&core, &mut s, "SELECT n FROM t LIMIT 5"),
        ["[SignedInteger(5)]"]
    );
    assert_eq!(
        query_rows(&core, &mut s, "SELECT n FROM s.t LIMIT 5"),
        ["[SignedInteger(50)]"]
    );
    // 参照中の SEQUENCE は削除できない。
    assert_eq!(code(&core, &mut s, "DROP SEQUENCE s.q"), "2BP01");
}

/// 同名の public 表と SCHEMA 配下の表は行・索引が混ざらない。
#[test]
fn same_name_in_public_and_schema_is_independent() {
    let (core, _g) = new_core("t27-objects-independent");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA a");
    ok(&core, &mut s, "CREATE SCHEMA b");
    for q in ["", "a.", "b."] {
        ok(
            &core,
            &mut s,
            &format!("CREATE TABLE {q}t (id BIGINT PRIMARY KEY, name TEXT)"),
        );
    }
    ok(&core, &mut s, "INSERT INTO t (id, name) VALUES (1, 'pub')");
    ok(
        &core,
        &mut s,
        "INSERT INTO a.t (id, name) VALUES (1, 'a1'), (2, 'a2')",
    );
    assert_eq!(count(&core, &mut s, "t"), "Integer(1)");
    assert_eq!(count(&core, &mut s, "a.t"), "Integer(2)");
    assert_eq!(count(&core, &mut s, "b.t"), "Integer(0)");
    // 同じ SCHEMA の中だけで名前が衝突する。
    assert_eq!(code(&core, &mut s, "CREATE TABLE a.t (id BIGINT)"), "42P07");
    ok(&core, &mut s, "DROP TABLE a.t");
    assert_eq!(count(&core, &mut s, "t"), "Integer(1)");
    assert_eq!(count(&core, &mut s, "b.t"), "Integer(0)");
    assert_eq!(code(&core, &mut s, "SELECT id FROM a.t LIMIT 1"), "42P01");
}
/// 文の実行結果を `Ok(Debug)`／`Err((code, message))` で返す（テナント指定つき）。
fn try_as(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, (String, String)> {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .map_err(|e| (e.wire_code().to_string(), e.to_string()))
}

fn query_rows(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<String> {
    match ok(core, s, sql) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| format!("{:?}", row.cells))
            .collect(),
        other => panic!("{sql}: expected Query, got {other:?}"),
    }
}

/// SEQUENCE の修飾名: 作成・`DEFAULT nextval`・`nextval('s.q')`・RENAME・DROP。public の同名 SEQUENCE と
/// 採番が混ざらない。
#[test]
fn sequences_in_schema_are_independent_of_public() {
    let (core, _g) = new_core("t27-objects-seq");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE SEQUENCE s.q START 100");
    ok(&core, &mut s, "CREATE SEQUENCE q START 1");
    // 同じ SCHEMA の中だけで衝突する。
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE s.q"), "42P07");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.u (id BIGINT PRIMARY KEY, n BIGINT DEFAULT nextval('s.q'))",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, n BIGINT DEFAULT nextval('q'))",
    );
    for (table, id) in [("s.u", 1), ("s.u", 2), ("u", 1), ("u", 2)] {
        ok(
            &core,
            &mut s,
            &format!("INSERT INTO {table} (id) VALUES ({id})"),
        );
    }
    ok(
        &core,
        &mut s,
        "INSERT INTO s.u (id, n) VALUES (3, nextval('s.q'))",
    );
    assert_eq!(
        query_rows(&core, &mut s, "SELECT id, n FROM s.u ORDER BY id LIMIT 10"),
        [
            "[SignedInteger(1), SignedInteger(100)]",
            "[SignedInteger(2), SignedInteger(101)]",
            "[SignedInteger(3), SignedInteger(102)]",
        ]
    );
    assert_eq!(
        query_rows(&core, &mut s, "SELECT id, n FROM u ORDER BY id LIMIT 10"),
        [
            "[SignedInteger(1), SignedInteger(1)]",
            "[SignedInteger(2), SignedInteger(2)]",
        ]
    );
    // RENAME は同じ SCHEMA に残る（public の同名へ化けない）。依存 DEFAULT があるうちは拒否。
    assert_eq!(
        code(&core, &mut s, "ALTER SEQUENCE s.q RENAME TO q2"),
        "0A000"
    );
    ok(&core, &mut s, "DROP TABLE s.u");
    ok(&core, &mut s, "ALTER SEQUENCE s.q RENAME TO q2");
    assert_eq!(code(&core, &mut s, "DROP SEQUENCE s.q"), "42P01");
    ok(&core, &mut s, "ALTER SEQUENCE s.q2 RESTART WITH 7");
    ok(&core, &mut s, "DROP SEQUENCE s.q2");
    // public 側は無傷。
    ok(&core, &mut s, "INSERT INTO u (id) VALUES (3)");
    // `public.q` は裸の名前と同じ。
    ok(&core, &mut s, "ALTER TABLE u ALTER COLUMN n DROP DEFAULT");
    ok(&core, &mut s, "DROP SEQUENCE public.q");
}

/// SERIAL・IDENTITY の暗黙 SEQUENCE は表と同じ SCHEMA に置かれ、表と一緒に消える。
#[test]
fn implicit_sequences_follow_the_table_schema() {
    let (core, _g) = new_core("t27-objects-serial");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGSERIAL PRIMARY KEY, name TEXT)",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGSERIAL PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO s.t (name) VALUES ('a'), ('b')");
    ok(&core, &mut s, "INSERT INTO t (name) VALUES ('x')");
    assert_eq!(
        query_rows(
            &core,
            &mut s,
            "SELECT id, name FROM s.t ORDER BY id LIMIT 10"
        )
        .len(),
        2
    );
    // 暗黙 SEQUENCE の名前は SCHEMA の中にある（public の同名と衝突しない）。
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE s.t_id_seq"), "42P07");
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE t_id_seq"), "42P07");
    ok(&core, &mut s, "DROP TABLE s.t");
    ok(&core, &mut s, "CREATE SEQUENCE s.t_id_seq");
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE t_id_seq"), "42P07");
}

/// ビュー: 本文は作成時に早期束縛され、ビュー経由の参照・更新ができる。public のビューから SCHEMA 配下へは
/// 依存できない（D7）。
#[test]
fn views_in_schema_bind_early_and_public_views_cannot_depend_on_schema() {
    let (core, _g) = new_core("t27-objects-view");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT, qty INTEGER)",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT, qty INTEGER)",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO s.t (id, name, qty) VALUES (1, 'a', 5), (2, 'b', 50)",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, qty) VALUES (9, 'pub', 99)",
    );
    // SCHEMA 配下のビューは SCHEMA 配下の表を指す。public の表を読むビューは D7 で `42501`。
    ok(
        &core,
        &mut s,
        "CREATE VIEW s.v AS SELECT id, name FROM s.t WHERE name = 'b'",
    );
    assert_eq!(
        code(&core, &mut s, "CREATE VIEW s.w AS SELECT id, name FROM t"),
        "42501"
    );
    assert_eq!(
        query_rows(
            &core,
            &mut s,
            "SELECT id, name FROM s.v ORDER BY id LIMIT 10"
        ),
        ["[SignedInteger(2), Text(\"b\")]"]
    );
    // 自動更新可能なビューへの書き込み。
    ok(&core, &mut s, "INSERT INTO s.v (id, name) VALUES (3, 'c')");
    assert_eq!(count(&core, &mut s, "s.t"), "Integer(3)");
    assert_eq!(count(&core, &mut s, "t"), "Integer(1)");
    // OR REPLACE と同名の衝突。
    ok(
        &core,
        &mut s,
        "CREATE OR REPLACE VIEW s.v AS SELECT id, name FROM s.t",
    );
    assert_eq!(
        code(&core, &mut s, "CREATE VIEW s.v AS SELECT id, name FROM s.t"),
        "42P07"
    );
    assert_eq!(
        code(&core, &mut s, "CREATE VIEW s.t AS SELECT id, name FROM s.t"),
        "42P07"
    );
    // 依存する表は RESTRICT で削除できない。
    assert_eq!(code(&core, &mut s, "DROP TABLE s.t"), "2BP01");
    // D7: public のビューは SCHEMA 配下を読めない（逆向きも拒否される）。
    assert_eq!(
        code(&core, &mut s, "CREATE VIEW pv AS SELECT id, name FROM s.t"),
        "42501"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW pm AS SELECT id, name FROM s.t"
        ),
        "42501"
    );
    // 名前が SCHEMA 配下の `DROP VIEW`。
    ok(&core, &mut s, "DROP VIEW s.v");
    assert_eq!(code(&core, &mut s, "DROP VIEW s.v"), "42P01");
    ok(&core, &mut s, "DROP VIEW IF EXISTS s.v");
    // MATERIALIZED VIEW の SCHEMA 配置は未対応（fail-closed）。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW s.m AS SELECT id FROM t"
        ),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW nope.m AS SELECT id FROM t"
        ),
        "3F000"
    );
}

/// D7: public と SCHEMA 配下の間の依存は両方向とも拒否される（外部キー・DEFAULT nextval・ビュー）。
#[test]
fn public_tables_cannot_depend_on_schema_objects() {
    let (core, _g) = new_core("t27-objects-d7");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    let parent = "(id BIGINT PRIMARY KEY, code TEXT UNIQUE)";
    ok(&core, &mut s, &format!("CREATE TABLE s.p {parent}"));
    ok(&core, &mut s, &format!("CREATE TABLE p {parent}"));
    ok(&core, &mut s, "CREATE SEQUENCE s.q");
    ok(&core, &mut s, "CREATE SEQUENCE q");
    ok(
        &core,
        &mut s,
        "CREATE TABLE c (id BIGINT PRIMARY KEY, pc TEXT)",
    );
    for sql in [
        "CREATE TABLE c2 (id BIGINT PRIMARY KEY, pc TEXT REFERENCES s.p (code))",
        "CREATE TABLE c3 (id BIGINT PRIMARY KEY, n BIGINT DEFAULT nextval('s.q'))",
        "ALTER TABLE c ADD FOREIGN KEY (pc) REFERENCES s.p (code)",
    ] {
        let (c, msg) = err(&core, &mut s, sql);
        assert_eq!(c, "42501", "{sql}");
        assert!(
            !msg.contains('#') && !msg.contains("s.q") && !msg.contains("s.p"),
            "{msg}"
        );
    }
    // 逆向き（SCHEMA 配下から public）も拒否する（public 側 DROP の依存判定が全テナントを走査し、
    // 他テナントの非公開の依存が存在情報になるため。固定文言で名前・内部 ID を含めない）。
    for sql in [
        "CREATE TABLE s.c (id BIGINT PRIMARY KEY, pc TEXT REFERENCES p (code))",
        "CREATE TABLE s.d (id BIGINT PRIMARY KEY, n BIGINT DEFAULT nextval('q'))",
        "CREATE VIEW s.pv AS SELECT id FROM p",
        "CREATE VIEW s.pj AS SELECT x.id FROM s.p AS x INNER JOIN p AS y ON x.id = y.id LIMIT 5",
    ] {
        let (c, msg) = err(&core, &mut s, sql);
        assert_eq!(c, "42501", "{sql}");
        assert!(!msg.contains('#'), "{msg}");
    }
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.c0 (id BIGINT PRIMARY KEY, n BIGINT)",
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "ALTER TABLE s.c0 ALTER COLUMN n SET DEFAULT nextval('q')"
        ),
        "42501"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "ALTER TABLE s.c0 ADD FOREIGN KEY (id) REFERENCES p (id)"
        ),
        "42501"
    );
    // 同じ SCHEMA 内の参照も許可。
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.e (id BIGINT PRIMARY KEY, pc TEXT REFERENCES s.p (code))",
    );
    // public の表は引き続き作れ、何も作られていない。
    assert_eq!(code(&core, &mut s, "SELECT id FROM c2 LIMIT 1"), "42P01");
    assert_eq!(code(&core, &mut s, "SELECT id FROM c3 LIMIT 1"), "42P01");
}

/// 他テナントの SCHEMA は存在しないものと同じ応答（3F000）。オブジェクト種別を問わない。
#[test]
fn other_tenants_schema_objects_are_unreachable_for_every_kind() {
    let (core, _g) = new_core("t27-objects-tenants");
    let mut alice = ddl_session();
    let mut bob = ddl_session();
    ok(&core, &mut alice, "CREATE SCHEMA a");
    ok(
        &core,
        &mut alice,
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(
        &core,
        &mut alice,
        "CREATE VIEW a.v AS SELECT id, name FROM a.t",
    );
    ok(&core, &mut alice, "CREATE SEQUENCE a.q");
    ok(
        &core,
        &mut alice,
        "INSERT INTO a.t (id, name) VALUES (1, 'secret')",
    );
    let statements = [
        "SELECT id FROM a.t LIMIT 1",
        "SELECT id FROM a.v LIMIT 1",
        "INSERT INTO a.t (id, name) VALUES (2, 'x')",
        "UPDATE a.t SET name = 'x' WHERE id = 1",
        "DELETE FROM a.t WHERE id = 1",
        "TRUNCATE a.t",
        "CREATE TABLE a.z (id BIGINT PRIMARY KEY)",
        "CREATE VIEW a.v2 AS SELECT id FROM a.t",
        "CREATE SEQUENCE a.q2",
        "CREATE INDEX i ON a.t (name)",
        "DROP TABLE a.t",
        "DROP VIEW a.v",
        "DROP SEQUENCE a.q",
        "DROP INDEX a.i",
        "ALTER TABLE a.t ADD COLUMN c TEXT",
        "ALTER SEQUENCE a.q RESTART",
        "INSERT INTO t2 (id, n) VALUES (1, nextval('a.q'))",
        "DROP SCHEMA a",
        "DROP SCHEMA a CASCADE",
    ];
    for sql in statements {
        // bob から見える SCHEMA は無い（存在しない名前 `nosuch` と同じ応答・同じ文言）。
        let theirs = try_as(&core, "bob", &mut bob, sql).expect_err(sql);
        let missing_sql = sql
            .replace("a.", "nosuch.")
            .replace("SCHEMA a", "SCHEMA nosuch");
        let missing = try_as(&core, "bob", &mut bob, &missing_sql).expect_err(sql);
        assert_eq!(theirs.0, "3F000", "{sql}: {theirs:?}");
        assert_eq!(
            theirs.1.replace("a", "X"),
            missing.1.replace("nosuch", "a").replace("a", "X"),
            "{sql}"
        );
        assert!(!theirs.1.contains('#'), "{sql}: {theirs:?}");
    }
    // alice の表は無傷。
    assert_eq!(count(&core, &mut alice, "a.t"), "Integer(1)");
    // bob が同名の SCHEMA を作っても別物（別の内部 ID）。
    ok_as(&core, "bob", &mut bob, "CREATE SCHEMA a");
    ok_as(
        &core,
        "bob",
        &mut bob,
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    assert_eq!(count_as(&core, "bob", &mut bob, "a.t"), "Integer(0)");
    assert_eq!(count(&core, &mut alice, "a.t"), "Integer(1)");
}

/// D7: 他テナントの SCHEMA 配下から public への依存は作れないため、public 側の `DROP` の結果
/// （RESTRICT・CASCADE とも）が他テナントの非公開オブジェクトの有無で変わらない（存在情報を漏らさない）。
#[test]
fn schema_to_public_dependency_cannot_leak_into_public_drop() {
    let (core, _g) = new_core("t27-objects-drop-oracle");
    let mut alice = ddl_session();
    let mut bob = ddl_session();
    ok_as(&core, "alice", &mut alice, "CREATE SCHEMA a");
    ok_as(
        &core,
        "alice",
        &mut alice,
        "CREATE TABLE p (id BIGINT PRIMARY KEY, code TEXT UNIQUE)",
    );
    ok_as(
        &core,
        "alice",
        &mut alice,
        "CREATE TABLE p2 (id BIGINT PRIMARY KEY)",
    );
    for sql in [
        "CREATE TABLE a.c (id BIGINT PRIMARY KEY, pc TEXT REFERENCES p (code))",
        "CREATE VIEW a.v AS SELECT id FROM p",
    ] {
        assert_eq!(
            run_as(&core, "alice", &mut alice, sql),
            "ERR 42501",
            "{sql}"
        );
    }
    // 依存が作られていないので、bob の public 側 DROP は alice の SCHEMA の有無と無関係に成功する。
    ok_as(&core, "bob", &mut bob, "DROP TABLE p");
    ok_as(&core, "bob", &mut bob, "DROP TABLE p2 CASCADE");
}

fn ok_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()))
}

fn count_as(core: &EngineCore, tenant: &str, s: &mut SessionState, table: &str) -> String {
    match ok_as(core, tenant, s, &format!("SELECT COUNT(*) FROM {table}")) {
        SqlOutcome::Query(r) => format!("{:?}", r.rows[0].cells[0]),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// エラー文言・EXPLAIN に内部キーを出さず、利用者が書いた `s.x` だけが出る。利用者が内部キーを直接
/// 書いても拒否される。
#[test]
fn internal_keys_never_reach_users_and_cannot_be_written() {
    let (core, _g) = new_core("t27-objects-hidden");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "CREATE VIEW s.v AS SELECT id, name FROM s.t");
    ok(&core, &mut s, "CREATE SEQUENCE s.q");
    for sql in [
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY)",
        "CREATE VIEW s.t AS SELECT id FROM s.t",
        "CREATE SEQUENCE s.t",
        "DROP TABLE s.v",
        "DROP VIEW s.t",
        "SELECT id FROM s.nope LIMIT 1",
        "INSERT INTO s.nope (id) VALUES (1)",
        "DROP TABLE s.t",
        "SELECT x.id FROM s.t LIMIT 1",
        "SELECT id FROM s.t WHERE nothere = 1 LIMIT 1",
        "SELECT id FROM s.t t1 JOIN s.t t1 ON t1.id = t1.id LIMIT 1",
    ] {
        let (c, msg) = match try_as(&core, TENANT, &mut s, sql) {
            Ok(_) => continue,
            Err(e) => e,
        };
        assert!(!msg.contains('#'), "{sql}: {c} {msg}");
        assert!(
            !msg.chars().any(|ch| ch.is_ascii_digit()) || !msg.contains("1."),
            "{sql}: {c} {msg}"
        );
    }
    let (_, msg) = err(&core, &mut s, "CREATE TABLE s.t (id BIGINT PRIMARY KEY)");
    assert!(msg.contains("s.t"), "{msg}");
    let (_, msg) = err(&core, &mut s, "SELECT id FROM s.nope LIMIT 1");
    assert!(msg.contains("s.nope"), "{msg}");
    // EXPLAIN。
    match ok(
        &core,
        &mut s,
        "EXPLAIN SELECT id FROM s.t WHERE id = 1 LIMIT 5",
    ) {
        SqlOutcome::Explain(r) => {
            let text = format!("{:?}", r.rows);
            assert!(!text.contains('#'), "{text}");
        }
        other => panic!("expected Explain, got {other:?}"),
    }
    // 内部キーを直接書く試みは、引用有無・文字列引数のいずれも拒否される。
    for sql in [
        "SELECT id FROM \"#1.t\" LIMIT 1",
        "SELECT id FROM #1.t LIMIT 1",
        "DROP TABLE \"#1.t\"",
        "CREATE TABLE \"#1.x\" (id BIGINT PRIMARY KEY)",
        "CREATE SEQUENCE \"#1.z\"",
        "INSERT INTO t2 (id, n) VALUES (1, nextval('#1.q'))",
        "SELECT setval('#1.q', 5)",
        "SELECT id FROM s.\"#1.t\" LIMIT 1",
    ] {
        let c = code(&core, &mut s, sql);
        assert!(
            ["42601", "42P01", "0A000"].contains(&c.as_str()),
            "{sql}: {c}"
        );
    }
    assert_eq!(count(&core, &mut s, "s.t"), "Integer(0)");
}

/// 派生キー（行ストア・索引・世代・台帳）が public の同名と混ざらない。
#[test]
fn derived_keys_do_not_mix_between_public_and_schema() {
    let (core, _g) = new_core("t27-objects-derived");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA a");
    for q in ["", "a."] {
        ok(
            &core,
            &mut s,
            &format!("CREATE TABLE {q}t (id BIGINT PRIMARY KEY, name TEXT UNIQUE, qty INTEGER)"),
        );
        ok(&core, &mut s, &format!("CREATE INDEX t_qty ON {q}t (qty)"));
    }
    // 台帳（USING OPERATION_ID）: 同じ operation_id を別の表へ使っても重複にならない。
    ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, qty) VALUES (1, 'x', 1) USING OPERATION_ID 'op1'",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO a.t (id, name, qty) VALUES (1, 'y', 2) USING OPERATION_ID 'op1'",
    );
    // 一意制約は表ごと（同じ値でも別表なら通り、同じ表なら衝突する）。
    ok(
        &core,
        &mut s,
        "INSERT INTO a.t (id, name, qty) VALUES (2, 'x', 3)",
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "INSERT INTO a.t (id, name, qty) VALUES (3, 'x', 3)"
        ),
        "23505"
    );
    // 行・索引経由の検索が混ざらない。
    assert_eq!(
        query_rows(
            &core,
            &mut s,
            "SELECT id, name FROM t WHERE qty = 1 LIMIT 10"
        ),
        ["[SignedInteger(1), Text(\"x\")]"]
    );
    assert_eq!(
        query_rows(
            &core,
            &mut s,
            "SELECT id, name FROM a.t WHERE qty = 1 LIMIT 10"
        )
        .len(),
        0
    );
    assert_eq!(
        query_rows(
            &core,
            &mut s,
            "SELECT id, name FROM a.t WHERE qty = 2 LIMIT 10"
        ),
        ["[SignedInteger(1), Text(\"y\")]"]
    );
    // TRUNCATE・DROP は片側だけに効く。
    ok(&core, &mut s, "TRUNCATE a.t USING OPERATION_ID 'tr1'");
    assert_eq!(count(&core, &mut s, "a.t"), "Integer(0)");
    assert_eq!(count(&core, &mut s, "t"), "Integer(1)");
    ok(&core, &mut s, "DROP TABLE a.t");
    assert_eq!(count(&core, &mut s, "t"), "Integer(1)");
    // 同名で作り直しても旧データを引き継がない。
    ok(
        &core,
        &mut s,
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT UNIQUE, qty INTEGER)",
    );
    assert_eq!(count(&core, &mut s, "a.t"), "Integer(0)");
    ok(
        &core,
        &mut s,
        "INSERT INTO a.t (id, name, qty) VALUES (1, 'y', 2) USING OPERATION_ID 'op1'",
    );
    // 分割実行 DML（述語形 UPDATE/DELETE）も表ごと。
    ok(
        &core,
        &mut s,
        "UPDATE a.t SET qty = 9 WHERE name = 'y' USING OPERATION_ID 'u1'",
    );
    assert_eq!(
        query_rows(&core, &mut s, "SELECT qty FROM t WHERE name = 'x' LIMIT 5"),
        ["[SignedInteger(1)]"]
    );
}

/// UDF の修飾名: `CREATE FUNCTION s.f(...)` と `s.f(x)` の呼び出し。public の同名とは独立。
#[test]
fn udfs_in_schema_are_registered_under_the_schema() {
    let (core, _g) = new_core("t27-objects-udf");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n INTEGER)",
    );
    ok(&core, &mut s, "INSERT INTO t (id, n) VALUES (1, 5), (2, 7)");
    ok(&core, &mut s, "CREATE FUNCTION s.dbl(x) AS x * 2");
    ok(&core, &mut s, "CREATE FUNCTION dbl(x) AS x * 3");
    assert_eq!(
        code(&core, &mut s, "CREATE FUNCTION s.dbl(x) AS x * 2"),
        "42723"
    );
    assert_eq!(
        query_rows(
            &core,
            &mut s,
            "SELECT id, s.dbl(n) AS d FROM t ORDER BY id LIMIT 10"
        ),
        [
            "[SignedInteger(1), Float(10.0)]",
            "[SignedInteger(2), Float(14.0)]"
        ]
    );
    assert_eq!(
        query_rows(
            &core,
            &mut s,
            "SELECT id, dbl(n) AS d FROM t ORDER BY id LIMIT 10"
        ),
        [
            "[SignedInteger(1), Float(15.0)]",
            "[SignedInteger(2), Float(21.0)]"
        ]
    );
    // 存在しない SCHEMA の関数呼び出し形の修飾は従来どおり構文エラー（42601）。SCHEMA を削除した後は
    // 解決できない。
    assert_eq!(
        code(&core, &mut s, "SELECT nope.dbl(n) AS d FROM t LIMIT 1"),
        "42601"
    );
    ok(&core, &mut s, "DROP SCHEMA s");
    assert_eq!(
        code(&core, &mut s, "SELECT s.dbl(n) AS d FROM t LIMIT 1"),
        "42601"
    );
    // 作り直した SCHEMA（別の内部 ID）では古い登録に当たらない。
    ok(&core, &mut s, "CREATE SCHEMA s");
    assert_eq!(
        code(&core, &mut s, "SELECT s.dbl(n) AS d FROM t LIMIT 1"),
        "42883"
    );
    // 組み込み SCHEMA・`public.` の修飾は従来どおり（組み込み関数や public の関数に化けない）。
    assert_eq!(
        code(&core, &mut s, "CREATE FUNCTION pg_catalog.f(x) AS x"),
        "42601"
    );
}

/// DROP SCHEMA ... CASCADE: 配下の表・索引・ビュー・SEQUENCE と、別 SCHEMA から依存するビューを削除する。
/// RESTRICT は 2BP01 のまま。結果は決定的（同じ構成を 2 回作って同じ結果になる）。
#[test]
fn drop_schema_cascade_removes_contents_and_external_dependents() {
    fn scenario(label: &str) -> Vec<String> {
        let (core, _g) = new_core(label);
        let mut s = ddl_session();
        let mut out = Vec::new();
        for sql in [
            "CREATE SCHEMA a",
            "CREATE SCHEMA b",
            "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT, qty INTEGER)",
            "CREATE TABLE a.s (id BIGSERIAL PRIMARY KEY, label TEXT)",
            "CREATE INDEX t_qty ON a.t (qty)",
            "CREATE VIEW a.v AS SELECT id, name FROM a.t",
            "CREATE VIEW a.v2 AS SELECT id, name FROM a.v",
            "CREATE SEQUENCE a.q",
            "CREATE TABLE b.keep (id BIGINT PRIMARY KEY)",
            "CREATE VIEW b.dep AS SELECT id, name FROM a.t",
            "CREATE TABLE t (id BIGINT PRIMARY KEY)",
            "INSERT INTO a.t (id, name, qty) VALUES (1, 'x', 1)",
            "DROP SCHEMA a",
            "DROP SCHEMA a RESTRICT",
            "DROP SCHEMA a CASCADE",
            "SELECT id FROM a.t LIMIT 1",
            "SELECT id, name FROM b.dep LIMIT 1",
            "SELECT COUNT(*) FROM b.keep",
            "SELECT COUNT(*) FROM t",
            "CREATE SCHEMA a",
            "SELECT id FROM a.t LIMIT 1",
            "CREATE TABLE a.t (id BIGINT PRIMARY KEY)",
            "SELECT COUNT(*) FROM a.t",
            "DROP SCHEMA IF EXISTS nosuch CASCADE",
            "DROP SCHEMA nosuch CASCADE",
            "DROP SCHEMA public CASCADE",
            "DROP SCHEMA a CASCADE",
            "DROP SCHEMA b CASCADE",
            "DROP SCHEMA b CASCADE",
        ] {
            out.push(
                run(&core, &mut s, sql)
                    .replace("#1.", "")
                    .replace("#2.", ""),
            );
        }
        out
    }
    let first = scenario("t27-objects-cascade-1");
    let second = scenario("t27-objects-cascade-2");
    assert_eq!(first, second, "CASCADE must be deterministic");
    let by_sql = |needle: &str| {
        first
            .iter()
            .zip([
                "CREATE SCHEMA a",
                "CREATE SCHEMA b",
                "CREATE TABLE a.t",
                "CREATE TABLE a.s",
                "CREATE INDEX t_qty",
                "CREATE VIEW a.v ",
                "CREATE VIEW a.v2",
                "CREATE SEQUENCE a.q",
                "CREATE TABLE b.keep",
                "CREATE VIEW b.dep",
                "CREATE TABLE t ",
                "INSERT",
                "DROP SCHEMA a",
                "DROP SCHEMA a RESTRICT",
                "DROP SCHEMA a CASCADE",
                "SELECT id FROM a.t",
                "SELECT id, name FROM b.dep",
                "SELECT COUNT(*) FROM b.keep",
                "SELECT COUNT(*) FROM t",
                "CREATE SCHEMA a AGAIN",
                "SELECT id FROM a.t AGAIN",
                "CREATE TABLE a.t AGAIN",
                "SELECT COUNT(*) FROM a.t",
                "DROP SCHEMA IF EXISTS nosuch CASCADE",
                "DROP SCHEMA nosuch CASCADE",
                "DROP SCHEMA public CASCADE",
                "DROP SCHEMA a CASCADE AGAIN",
                "DROP SCHEMA b CASCADE",
                "DROP SCHEMA b CASCADE AGAIN",
            ])
            .find(|(_, k)| *k == needle)
            .map(|(v, _)| v.clone())
            .unwrap_or_else(|| panic!("{needle}"))
    };
    assert_eq!(by_sql("DROP SCHEMA a"), "ERR 2BP01");
    assert_eq!(by_sql("DROP SCHEMA a RESTRICT"), "ERR 2BP01");
    assert!(by_sql("DROP SCHEMA a CASCADE").starts_with("DropSchema"));
    assert_eq!(by_sql("SELECT id FROM a.t"), "ERR 3F000");
    // 別 SCHEMA のビューは連鎖削除され、別 SCHEMA の表と public の表は無傷。
    assert_eq!(by_sql("SELECT id, name FROM b.dep"), "ERR 42P01");
    assert!(by_sql("SELECT COUNT(*) FROM b.keep").contains("Integer(0)"));
    assert!(by_sql("SELECT COUNT(*) FROM t").contains("Integer(0)"));
    // 同名で作り直した SCHEMA は空で、旧オブジェクトは戻らない（内部 ID を再利用しない）。
    assert_eq!(by_sql("SELECT id FROM a.t AGAIN"), "ERR 42P01");
    assert!(by_sql("SELECT COUNT(*) FROM a.t").contains("Integer(0)"));
    assert!(by_sql("DROP SCHEMA IF EXISTS nosuch CASCADE").starts_with("DropSchema"));
    assert_eq!(by_sql("DROP SCHEMA nosuch CASCADE"), "ERR 3F000");
    assert_eq!(by_sql("DROP SCHEMA public CASCADE"), "ERR 42501");
    assert!(by_sql("DROP SCHEMA b CASCADE").starts_with("DropSchema"));
    assert_eq!(by_sql("DROP SCHEMA b CASCADE AGAIN"), "ERR 3F000");
}

/// SEQUENCE だけを持つ SCHEMA は RESTRICT で拒否され、CASCADE で消える。DDL 権限のない主体は拒否。
#[test]
fn sequence_only_schema_is_restricted_and_cascades() {
    let (core, _g) = new_core("t27-objects-seqonly");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA a");
    ok(&core, &mut s, "CREATE SEQUENCE a.q");
    assert_eq!(code(&core, &mut s, "DROP SCHEMA a"), "2BP01");
    let mut plain = SessionState::default();
    assert_eq!(code(&core, &mut plain, "DROP SCHEMA a CASCADE"), "42501");
    ok(&core, &mut s, "DROP SCHEMA a CASCADE");
    ok(&core, &mut s, "CREATE SCHEMA a");
    ok(&core, &mut s, "CREATE SEQUENCE a.q");
}

/// 外部の表が SCHEMA 配下の SEQUENCE を DEFAULT で参照している場合、CASCADE でも拒否する（fail-closed）。
#[test]
fn cascade_refuses_when_an_external_default_uses_a_schema_sequence() {
    let (core, _g) = new_core("t27-objects-cascade-default");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA a");
    ok(&core, &mut s, "CREATE SCHEMA b");
    ok(&core, &mut s, "CREATE SEQUENCE a.q");
    ok(
        &core,
        &mut s,
        "CREATE TABLE b.t (id BIGINT PRIMARY KEY, n BIGINT DEFAULT nextval('a.q'))",
    );
    assert_eq!(code(&core, &mut s, "DROP SCHEMA a CASCADE"), "2BP01");
    ok(&core, &mut s, "DROP TABLE b.t");
    ok(&core, &mut s, "DROP SCHEMA a CASCADE");
}

/// CREATE INDEX の索引名は修飾できない（PG と同じ）。索引は対象表の SCHEMA に属し、`DROP INDEX s.i` で消える。
#[test]
fn indexes_belong_to_the_table_schema() {
    let (core, _g) = new_core("t27-objects-index");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    assert_eq!(
        code(&core, &mut s, "CREATE INDEX s.i ON s.t (name)"),
        "42601"
    );
    ok(&core, &mut s, "CREATE INDEX i ON s.t (name)");
    // 同名の索引は public 側にも作れる（SCHEMA が違う）。同じ SCHEMA では衝突する。
    ok(&core, &mut s, "CREATE INDEX i ON t (name)");
    assert_eq!(code(&core, &mut s, "CREATE INDEX i ON s.t (name)"), "42P07");
    // 修飾なしの `DROP INDEX i` は public 側。
    ok(&core, &mut s, "DROP INDEX i");
    assert_eq!(code(&core, &mut s, "DROP INDEX i"), "42704");
    ok(&core, &mut s, "DROP INDEX s.i");
    assert_eq!(code(&core, &mut s, "DROP INDEX s.i"), "42704");
    ok(&core, &mut s, "DROP INDEX IF EXISTS s.i");
    // 一意索引。
    ok(&core, &mut s, "CREATE UNIQUE INDEX u ON s.t (name)");
    ok(&core, &mut s, "INSERT INTO s.t (id, name) VALUES (1, 'a')");
    assert_eq!(
        code(&core, &mut s, "INSERT INTO s.t (id, name) VALUES (2, 'a')"),
        "23505"
    );
    // 表と一緒に消える。
    ok(&core, &mut s, "DROP TABLE s.t");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "CREATE INDEX u ON s.t (name)");
}

/// ALTER TABLE ... RENAME TO は同じ SCHEMA に残る。
#[test]
fn rename_table_stays_in_the_schema() {
    let (core, _g) = new_core("t27-objects-rename");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "INSERT INTO s.t (id, name) VALUES (1, 'a')");
    ok(&core, &mut s, "ALTER TABLE s.t RENAME TO t2");
    assert_eq!(count(&core, &mut s, "s.t2"), "Integer(1)");
    assert_eq!(code(&core, &mut s, "SELECT id FROM t2 LIMIT 1"), "42P01");
    assert_eq!(code(&core, &mut s, "SELECT id FROM s.t LIMIT 1"), "42P01");
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE s.t2 RENAME TO s.t3"),
        "42601"
    );
}

/// MATERIALIZED VIEW と ENUM 型の SCHEMA 配置は未対応（fail-closed のまま）。
#[test]
fn matview_and_enum_type_placement_stay_unsupported() {
    let (core, _g) = new_core("t27-objects-unsupported");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(&core, &mut s, "CREATE TABLE t (id BIGINT PRIMARY KEY)");
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW s.m AS SELECT id FROM t"
        ),
        "0A000"
    );
    assert_eq!(
        code(&core, &mut s, "CREATE TYPE s.mood AS ENUM ('a')"),
        "42601"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.mood)"
        ),
        "42601"
    );
}

/// Rust API の生の表名（`EngineCore::insert_row` 等）は裸の識別子だけを受理し、SCHEMA 配下の内部キーを
/// 直接指定して（他テナントを含む）SCHEMA 配下の表を操作する経路を作らない。一覧 API が内部キーを
/// 返さないことは `catalog::namespace` の単体テスト（`list_apis_do_not_expose_schema_keys`）。
#[test]
fn raw_table_names_never_reach_schema_tables() {
    use engine::storage::RowInput;
    let (core, _g) = new_core("t27-objects-raw");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA a");
    ok(
        &core,
        &mut s,
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE pubt (id BIGINT PRIMARY KEY, name TEXT)",
    );
    ok(&core, &mut s, "CREATE INDEX i ON a.t (name)");
    let row = RowInput {
        tenant_id: TENANT,
        visibility: Visibility::Private,
        embedding: &[],
        metadata: &[],
    };
    for table in ["#1.t", "#1", "a.t", "\"#1.t\""] {
        assert!(core.insert_row(&ctx(TENANT), table, 1, &row, None).is_err());
        assert!(core.update_row(&ctx(TENANT), table, 1, &row, None).is_err());
        assert!(core.delete_row(&ctx(TENANT), table, 1, None).is_err());
    }
}

/// 別名なしの UDF 呼び出しの既定列名は公開名（内部キー `#<id>.f` を出さない）。SCHEMA 配下の表を
/// `public` へ戻す `SET SCHEMA` は何も変えず成功せず 42501 で拒否する（Issue #1717）。
#[test]
fn default_udf_column_name_is_public_and_set_schema_to_public_is_rejected() {
    let (core, _g) = new_core("t27-objects-udf-name");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, n INTEGER)",
    );
    ok(&core, &mut s, "INSERT INTO s.t (id, n) VALUES (1, 5)");
    ok(&core, &mut s, "CREATE FUNCTION s.f(x) AS x");
    match ok(&core, &mut s, "SELECT s.f(n) FROM s.t LIMIT 1") {
        SqlOutcome::Query(r) => {
            let dbg = format!("{:?}", r.columns);
            assert!(!dbg.contains('#'), "{dbg}");
            assert!(dbg.contains("\"f\""), "{dbg}");
        }
        other => panic!("expected Query, got {other:?}"),
    }
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE s.t SET SCHEMA public"),
        "42501"
    );
}

/// D7: SCHEMA 配下の表は public の ENUM 型（配列を含む）を使えない（`42501`）。使えると、他テナントの
/// `DROP TYPE` が alice の表の有無で成功／`2BP01` に分かれ存在情報が漏れる。public の表は従来どおり使える。
#[test]
fn schema_tables_cannot_depend_on_public_enum_types() {
    let (core, _g) = new_core("t27-objects-enum-dep");
    let mut alice = ddl_session();
    let mut bob = ddl_session();
    for sql in [
        "CREATE SCHEMA s",
        "CREATE TYPE mood AS ENUM ('a', 'b')",
        "CREATE TABLE s.base (id BIGINT PRIMARY KEY, n TEXT)",
    ] {
        ok_as(&core, "alice", &mut alice, sql);
    }
    for sql in [
        "CREATE TABLE s.t (m mood)",
        "CREATE TABLE s.t2 (m mood[])",
        "ALTER TABLE s.base ADD COLUMN m mood",
        "ALTER TABLE s.base ADD COLUMN ms mood[]",
        "ALTER TABLE s.base ALTER COLUMN n TYPE mood",
    ] {
        assert_eq!(
            run_as(&core, "alice", &mut alice, sql),
            "ERR 42501",
            "{sql}"
        );
    }
    // public の表は従来どおり ENUM を使える。
    ok_as(&core, "alice", &mut alice, "CREATE TABLE pt (m mood)");
    ok_as(&core, "alice", &mut alice, "DROP TABLE pt");
    // 依存が作られていないので、他テナントの DROP TYPE の結果は alice の SCHEMA 表の有無と無関係。
    ok_as(&core, "bob", &mut bob, "DROP TYPE mood");
}

/// 一時表は SCHEMA 修飾で作れない。存在する SCHEMA でも `public.t` と同じ拒否になる
/// （内部キーへ書き換わって後段の識別子エラーにならない）。
#[test]
fn temp_table_cannot_be_created_in_an_existing_schema() {
    let (core, _g) = new_core("t27-objects-temp-schema");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    let public = code(
        &core,
        &mut s,
        "CREATE TEMP TABLE public.t (id BIGINT PRIMARY KEY)",
    );
    let schema = code(
        &core,
        &mut s,
        "CREATE TEMP TABLE s.t (id BIGINT PRIMARY KEY)",
    );
    assert_eq!(schema, public);
}

/// COPY 完了処理（`commit_copy_in`）のエラーにも内部キー（`#<id>.t`）が出ない（ADR D2）。
/// COPY 開始後に表が削除された場合でも、利用者向けの名前だけを返す。
#[test]
fn copy_commit_errors_hide_internal_keys() {
    let (core, _g) = new_core("t27-objects-copy-commit");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA a");
    ok(
        &core,
        &mut s,
        "CREATE TABLE a.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    let plan = core
        .begin_copy(&ctx(TENANT), &s, "COPY a.t (id, name) FROM STDIN")
        .expect("copy from");
    let engine::core::CopyPlan::From(mut session) = plan else {
        panic!("expected CopyPlan::From");
    };
    session.feed(b"1\tx\n").expect("feed");
    ok(&core, &mut s, "DROP TABLE a.t");
    let batch = session.finish().expect("finish");
    let e = core
        .commit_copy_in(&ctx(TENANT), batch)
        .expect_err("table was dropped");
    assert!(!e.to_string().contains('#'), "{e}");
}

/// 既存表への `CREATE TABLE IF NOT EXISTS` は意味検査（SCHEMA 依存検査）より前に no-op 成功する。
#[test]
fn create_if_not_exists_on_existing_table_is_noop_before_dependency_check() {
    let (core, _g) = new_core("t27-objects-ine");
    let mut s = ddl_session();
    ok(&core, &mut s, "CREATE SCHEMA s");
    ok(
        &core,
        &mut s,
        "CREATE TABLE s.p (id BIGINT PRIMARY KEY, code TEXT UNIQUE)",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE c (id BIGINT PRIMARY KEY, pc TEXT)",
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE IF NOT EXISTS c (id BIGINT PRIMARY KEY, pc TEXT REFERENCES s.p (code))",
    );
    // 実際に作成する場合は従来どおり拒否する。
    let c = code(
        &core,
        &mut s,
        "CREATE TABLE IF NOT EXISTS c9 (id BIGINT PRIMARY KEY, pc TEXT REFERENCES s.p (code))",
    );
    assert_eq!(c, "42501");
}
