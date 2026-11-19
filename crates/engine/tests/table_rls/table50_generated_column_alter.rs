//! 生成列が参照する列の変更・削除・改名の依存検査の結合テスト（Issue #3331。ポインタ:
//! `docs/spec/04-behavior/` の TABLE-50 (3)・TABLE-38・RECOVER-11・TASK-345、設計記録:
//! `docs/design/generated-column.md`）。
//!
//! 検証の軸は 5 つ。(1) 参照列の `RENAME COLUMN` が生成式へ追随し、再起動後も保たれること。
//! (2) `DROP COLUMN` の `RESTRICT`／無指定が `2BP01`、`CASCADE` が依存する生成列を連鎖削除し、
//! 連鎖できない依存があれば何も削除せず `2BP01` になること。(3) `ALTER COLUMN ... DROP EXPRESSION` が
//! 生成列を通常の列へ変え、書き込みの暫定停止を外すこと。(4) 最終形として残る拒否
//! （参照列・生成列自身の型変更の `0A000`、`SET EXPRESSION` の `0A000`、`SET`／`DROP DEFAULT` の `42601`）。
//! (5) 権限ゲートと、拒否文言が他テナント・表の情報を含まないこと。

use engine::catalog::ddl_export::SchemaDdlScope;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "tenant-a";
const OTHER: &str = "tenant-b";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
}

fn fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    (path, guard, core)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {} {e}", e.wire_code()));
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn message(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.to_string(),
    }
}

fn dump(core: &EngineCore, tenant: &str) -> Vec<String> {
    let mut out = Vec::new();
    core.export_schema_ddl(&SchemaDdlScope::shared_and_tenant(tenant), |st| {
        out.push(st.sql.to_string());
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("export");
    out
}

fn create_stmt_of(core: &EngineCore, tenant: &str, table: &str) -> String {
    dump(core, tenant)
        .into_iter()
        .find(|s| s.starts_with("CREATE TABLE") && s.contains(&format!("\"{table}\"")))
        .unwrap_or_else(|| panic!("CREATE TABLE for {table} must be dumped"))
}

/// 参照列の改名は生成式と参照列へ追随し、再起動後・ダンプの往復後も保たれる。
#[test]
fn rename_of_a_referenced_column_follows_the_expression() {
    let path = unique_db_path("t50a-rename");
    let _g = CleanupGuard(path.clone());
    {
        let core = open_core(&path);
        ok(
            &core,
            TENANT,
            "CREATE TABLE g (a INTEGER, other TEXT, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, \
             c INTEGER GENERATED ALWAYS AS (a + 1) STORED)",
        );
        ok(&core, TENANT, "ALTER TABLE g RENAME COLUMN a TO a2");
        let create = create_stmt_of(&core, TENANT, "g");
        assert!(create.contains("(a2 * 2)"), "{create}");
        assert!(create.contains("(a2 + 1)"), "{create}");
        assert!(!create.contains("\"a\""), "{create}");
        // 改名後の参照列は新しい名前の DROP が `2BP01` になることで確かめる。
        assert_eq!(code(&core, TENANT, "ALTER TABLE g DROP COLUMN a2"), "2BP01");
    }
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("g").expect("schema");
    let b = schema.columns.iter().find(|c| c.name == "b").expect("b");
    assert!(b.generation_expression().is_some_and(|e| e.contains("a2")));
    drop(storage);

    // ダンプの往復で別 DB へ復元でき、再ダンプが一致する。
    let core = open_core(&path);
    let first = dump(&core, TENANT);
    drop(core);
    let restore_path = unique_db_path("t50a-rename-restore");
    let _g2 = CleanupGuard(restore_path.clone());
    let restored = open_core(&restore_path);
    for sql in &first {
        ok(&restored, TENANT, sql);
    }
    assert_eq!(dump(&restored, TENANT), first);
}

/// 列名の入れ替え（a→tmp、b→a、tmp→b）でも連鎖置換が起きない。生成列自身の改名・psql 形も通る。
#[test]
fn rename_swap_does_not_chain_and_psql_form_is_accepted() {
    let (_p, _g, core) = fixture("t50a-swap");
    ok(
        &core,
        TENANT,
        "CREATE TABLE sw (a INTEGER, b INTEGER, c INTEGER GENERATED ALWAYS AS (a - b) STORED)",
    );
    let before = create_stmt_of(&core, TENANT, "sw");
    assert!(before.contains("(a - b)"), "{before}");
    ok(&core, TENANT, "ALTER TABLE sw RENAME COLUMN a TO tmp;");
    ok(&core, TENANT, "ALTER TABLE sw RENAME COLUMN b TO a;");
    ok(&core, TENANT, "ALTER TABLE sw RENAME COLUMN tmp TO b;");
    let after = create_stmt_of(&core, TENANT, "sw");
    assert!(after.contains("(b - a)"), "{after}");
    // 生成列自身の改名は式に影響しない。
    ok(&core, TENANT, "ALTER TABLE sw RENAME COLUMN c TO c2");
    let renamed = create_stmt_of(&core, TENANT, "sw");
    assert!(renamed.contains("(b - a)"), "{renamed}");
}

/// ゾーン無しの TIMESTAMPTZ リテラルは宣言時のセッション TZ で確定値へ畳まれる。別の TZ のセッションでの
/// 改名は識別子だけを変え、リテラルの描画を変えない（再束縛の再畳み込みが no-op であることの固定）。
#[test]
fn rename_does_not_change_literal_rendering_under_another_time_zone() {
    let (_p, _g, core) = fixture("t50a-tz");
    let mut session = SessionState::default();
    session.allow_ddl();
    let mut exec = |sql: &str| {
        core.execute_sql_in_session(&ctx(TENANT), &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed: {} {e}", e.wire_code()));
    };
    exec("SET TimeZone = 'Asia/Tokyo'");
    exec(
        "CREATE TABLE tz (a INTEGER, \
         b TIMESTAMPTZ GENERATED ALWAYS AS (CASE WHEN a > 0 THEN TIMESTAMPTZ '2024-01-01 00:00:00' ELSE NULL END) STORED)",
    );
    exec("SET TimeZone = 'America/New_York'");
    exec("ALTER TABLE tz RENAME COLUMN a TO a2");
    let after = create_stmt_of(&core, TENANT, "tz");
    assert!(after.contains("a2 > 0"), "{after}");
    // 定義時（Asia/Tokyo）に確定した UTC 値が保たれる（New_York の解釈へ変わらない）。
    assert!(after.contains("2023-12-31"), "{after}");
}

/// 参照列の DROP は無指定・`RESTRICT` とも `2BP01`。`CASCADE` は依存する生成列を連鎖削除する。
#[test]
fn drop_column_restrict_and_cascade() {
    let (_p, _g, core) = fixture("t50a-drop");
    let ddl =
        "CREATE TABLE g (a INTEGER, other TEXT, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, \
               c TEXT GENERATED ALWAYS AS (other || 'x') STORED)";
    ok(&core, TENANT, ddl);
    ok(&core, TENANT, "CREATE INDEX g_b_idx ON g (b)");
    assert_eq!(code(&core, TENANT, "ALTER TABLE g DROP COLUMN a"), "2BP01");
    assert_eq!(
        code(&core, TENANT, "ALTER TABLE g DROP COLUMN a RESTRICT"),
        "2BP01"
    );
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE g DROP COLUMN a CASCADE RESTRICT"
        ),
        "42601"
    );
    // 拒否後は何も変わらない。
    let before = create_stmt_of(&core, TENANT, "g");
    assert!(before.contains("\"a\""), "{before}");
    ok(&core, TENANT, "ALTER TABLE g DROP COLUMN a CASCADE;");
    let after = create_stmt_of(&core, TENANT, "g");
    assert!(!after.contains("\"a\""), "{after}");
    assert!(!after.contains("\"b\""), "{after}");
    assert!(after.contains("\"c\""), "{after}");
    // 連鎖削除した生成列の索引宣言も消える（同名の索引を作り直せる）。
    assert!(
        !dump(&core, TENANT).iter().any(|s| s.contains("g_b_idx")),
        "index declaration must be removed"
    );
    // 依存の無い列（生成列自身）と、依存が消えた後の列の CASCADE／RESTRICT は通常の DROP と同じ。
    assert_eq!(
        code(&core, TENANT, "ALTER TABLE g DROP COLUMN other"),
        "2BP01"
    );
    ok(&core, TENANT, "ALTER TABLE g DROP COLUMN c RESTRICT");
    ok(&core, TENANT, "ALTER TABLE g ADD COLUMN extra INTEGER");
    ok(&core, TENANT, "ALTER TABLE g DROP COLUMN other CASCADE");
}

/// 生成列が 0 本になった表は通常の表へ戻り、生成列の計算・直接指定の拒否が外れる。再起動後も一致する。
#[test]
fn cascade_dropping_the_last_generated_column_unblocks_writes() {
    let path = unique_db_path("t50a-unblock");
    let _g = CleanupGuard(path.clone());
    {
        let core = open_core(&path);
        ok(
            &core,
            TENANT,
            "CREATE TABLE g (a INTEGER, z INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
        );
        ok(&core, TENANT, "INSERT INTO g (a, z) VALUES (1, 2)");
        assert_eq!(
            code(&core, TENANT, "INSERT INTO g (a, z, b) VALUES (1, 2, 3)"),
            "428C9"
        );
        ok(&core, TENANT, "ALTER TABLE g DROP COLUMN a CASCADE");
        ok(&core, TENANT, "INSERT INTO g (z) VALUES (2)");
    }
    let core = open_core(&path);
    let create = create_stmt_of(&core, TENANT, "g");
    assert!(!create.contains("GENERATED"), "{create}");
    ok(&core, TENANT, "INSERT INTO g (z) VALUES (3)");
}

/// 連鎖できない依存（連鎖先・対象列自身の CHECK・UNIQUE・ビュー・PRIMARY KEY）があれば `CASCADE` でも
/// `2BP01` で、何も削除されない。
#[test]
fn cascade_rejects_dependencies_it_cannot_follow_and_drops_nothing() {
    let (_p, _g, core) = fixture("t50a-cascade-reject");
    for (setup, label) in [
        (
            "CREATE TABLE c1 (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) STORED, CHECK (b > 0))",
            "c1",
        ),
        (
            "CREATE TABLE c2 (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) STORED, UNIQUE (b))",
            "c2",
        ),
        (
            "CREATE TABLE c3 (a INTEGER PRIMARY KEY, b INTEGER GENERATED ALWAYS AS (a + 1) STORED)",
            "c3",
        ),
        (
            "CREATE TABLE c4 (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) STORED, CHECK (a > 0))",
            "c4",
        ),
    ] {
        ok(&core, TENANT, setup);
        let before = create_stmt_of(&core, TENANT, label);
        assert_eq!(
            code(&core, TENANT, &format!("ALTER TABLE {label} DROP COLUMN a CASCADE")),
            "2BP01",
            "{setup}"
        );
        assert_eq!(create_stmt_of(&core, TENANT, label), before, "{setup}");
    }
    ok(
        &core,
        TENANT,
        "CREATE TABLE c5 (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) STORED)",
    );
    ok(&core, TENANT, "CREATE VIEW c5_v AS SELECT b FROM c5");
    let before = create_stmt_of(&core, TENANT, "c5");
    assert_eq!(
        code(&core, TENANT, "ALTER TABLE c5 DROP COLUMN a CASCADE"),
        "2BP01"
    );
    assert_eq!(create_stmt_of(&core, TENANT, "c5"), before);
}

/// 複数アクションは原子的に適用される。連鎖で消えた列への後続アクションは `42703` で全体を破棄する。
#[test]
fn multiple_actions_with_cascade_are_atomic() {
    let (_p, _g, core) = fixture("t50a-multi");
    ok(
        &core,
        TENANT,
        "CREATE TABLE m (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) STORED, keep INTEGER)",
    );
    let before = create_stmt_of(&core, TENANT, "m");
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE m DROP COLUMN a CASCADE, ALTER COLUMN b DROP NOT NULL"
        ),
        "42703"
    );
    assert_eq!(create_stmt_of(&core, TENANT, "m"), before);
    ok(
        &core,
        TENANT,
        "ALTER TABLE m DROP COLUMN a CASCADE, ADD COLUMN z INTEGER",
    );
    let after = create_stmt_of(&core, TENANT, "m");
    assert!(after.contains("\"z\""), "{after}");
    assert!(!after.contains("\"a\""), "{after}");
    assert!(!after.contains("\"b\""), "{after}");
}

/// `DROP EXPRESSION` は生成列を通常の列へ変え、参照列の DROP・RENAME・型変更が通るようになる。
#[test]
fn drop_expression_turns_a_generated_column_into_a_plain_column() {
    let (_p, _g, core) = fixture("t50a-dropexpr");
    ok(
        &core,
        TENANT,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED NOT NULL)",
    );
    assert_eq!(
        code(&core, TENANT, "ALTER TABLE g ALTER COLUMN a TYPE BIGINT"),
        "0A000"
    );
    ok(
        &core,
        TENANT,
        "ALTER TABLE g ALTER COLUMN b DROP EXPRESSION;",
    );
    let create = create_stmt_of(&core, TENANT, "g");
    assert!(!create.contains("GENERATED"), "{create}");
    assert!(create.contains("NOT NULL"), "{create}");
    // 書き込みの暫定停止が外れ、参照列だった列の変更が通る。
    ok(&core, TENANT, "INSERT INTO g (a, b) VALUES (1, 2)");
    ok(&core, TENANT, "ALTER TABLE g ALTER COLUMN a TYPE BIGINT");
    ok(&core, TENANT, "ALTER TABLE g RENAME COLUMN a TO a2");
    ok(&core, TENANT, "ALTER TABLE g DROP COLUMN a2");
}

/// 生成列が 2 本ある表で 1 本だけ外しても、残りの生成列は保たれ生成列の計算も続く。
#[test]
fn drop_expression_on_one_of_two_generated_columns_keeps_the_other() {
    let (_p, _g, core) = fixture("t50a-dropexpr-two");
    ok(
        &core,
        TENANT,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, \
         c INTEGER GENERATED ALWAYS AS (a + 1) STORED)",
    );
    ok(
        &core,
        TENANT,
        "ALTER TABLE g ALTER COLUMN b DROP EXPRESSION",
    );
    let create = create_stmt_of(&core, TENANT, "g");
    assert!(create.contains("GENERATED"), "{create}");
    ok(&core, TENANT, "INSERT INTO g (a) VALUES (1)");
    assert_eq!(
        code(&core, TENANT, "INSERT INTO g (a, c) VALUES (1, 2)"),
        "428C9"
    );
    assert_eq!(code(&core, TENANT, "ALTER TABLE g DROP COLUMN a"), "2BP01");
}

/// `DROP EXPRESSION` の拒否と no-op。
#[test]
fn drop_expression_rejections_and_noop() {
    let (_p, _g, core) = fixture("t50a-dropexpr-reject");
    ok(
        &core,
        TENANT,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    let before = create_stmt_of(&core, TENANT, "g");
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE g ALTER COLUMN a DROP EXPRESSION"
        ),
        "55000"
    );
    assert_eq!(
        message(
            &core,
            TENANT,
            "ALTER TABLE g ALTER COLUMN a DROP EXPRESSION"
        ),
        "column is not a stored generated column"
    );
    ok(
        &core,
        TENANT,
        "ALTER TABLE g ALTER COLUMN a DROP EXPRESSION IF EXISTS;",
    );
    assert_eq!(create_stmt_of(&core, TENANT, "g"), before);
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE g ALTER COLUMN nope DROP EXPRESSION"
        ),
        "42703"
    );
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE g ALTER COLUMN id DROP EXPRESSION"
        ),
        "42601"
    );
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE nope ALTER COLUMN a DROP EXPRESSION"
        ),
        "42P01"
    );
}

/// 最終形として残る拒否（`intentional`）。
#[test]
// 生成列自身の型変更と `SET EXPRESSION` は計算点・全行の書き直しが無い間は受理しない（設計記録の対象外）。
// intentional: docs/design/generated-column.md
fn remaining_rejections_are_fail_closed() {
    let (_p, _g, core) = fixture("t50a-remaining");
    ok(
        &core,
        TENANT,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    for (sql, expected) in [
        ("ALTER TABLE g ALTER COLUMN a TYPE BIGINT", "0A000"),
        ("ALTER TABLE g ALTER COLUMN a TYPE BIGINT;", "0A000"),
        ("ALTER TABLE g ALTER COLUMN b TYPE BIGINT", "0A000"),
        (
            "ALTER TABLE g ALTER COLUMN b SET DATA TYPE BIGINT USING b",
            "0A000",
        ),
        (
            "ALTER TABLE g ALTER COLUMN b SET EXPRESSION AS (a + 1)",
            "0A000",
        ),
        (
            "ALTER TABLE g ALTER COLUMN b SET EXPRESSION AS (a + 1);",
            "0A000",
        ),
        ("ALTER TABLE g ALTER COLUMN b SET DEFAULT 1", "42601"),
        ("ALTER TABLE g ALTER COLUMN b DROP DEFAULT", "42601"),
        ("ALTER TABLE g DROP COLUMN a;", "2BP01"),
        ("ALTER TABLE g DROP COLUMN IF EXISTS a", "42601"),
        ("ALTER TABLE g ALTER COLUMN b DROP EXPRESSION IF", "42601"),
    ] {
        assert_eq!(code(&core, TENANT, sql), expected, "{sql}");
    }
}

/// DDL 権限の無いセッションは新しい形をすべて `42501` で拒否し、表の実在で結果が変わらない。
/// 拒否文言は他テナント・表の情報を含まない。
#[test]
fn permission_gate_and_messages_do_not_leak() {
    let (_p, _g, core) = fixture("t50a-gate");
    ok(
        &core,
        TENANT,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    for table in ["g", "no_such_table"] {
        for tail in [
            "RENAME COLUMN a TO a2",
            "DROP COLUMN a CASCADE",
            "DROP COLUMN a RESTRICT",
            "ALTER COLUMN b DROP EXPRESSION",
            "ALTER COLUMN b DROP EXPRESSION IF EXISTS",
        ] {
            let sql = format!("ALTER TABLE {table} {tail}");
            let mut session = SessionState::default();
            let err = core
                .execute_sql_in_session(&ctx(OTHER), &mut session, &sql)
                .expect_err("must be rejected without the DDL privilege");
            assert_eq!(err.wire_code(), "42501", "{sql}");
        }
    }
    for sql in [
        "ALTER TABLE g DROP COLUMN a",
        "ALTER TABLE g ALTER COLUMN a TYPE BIGINT",
        "ALTER TABLE g ALTER COLUMN a DROP EXPRESSION",
    ] {
        let m = message(&core, OTHER, sql);
        assert!(!m.contains(TENANT), "{m}");
        assert!(!m.contains(OTHER), "{m}");
        assert!(!m.contains("\"g\""), "{m}");
    }
}
