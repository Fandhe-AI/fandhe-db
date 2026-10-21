//! `INSERT` の `DEFAULT VALUES`・値位置の `DEFAULT`・列リスト省略（SQL-50、Issue #1730）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql.md` SQL-50、`data-model.md` TABLE-16・TABLE-24、
//! `recovery.md` RECOVER-10・RECOVER-11、`rls.md` RLS-9・RLS-10。設計記録は
//! `docs/design/not-null-default.md`「Issue #1730 追記」。
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路（`EngineCore::execute_sql_in_session`）で、
//! (1) 3 形の受理と列 DEFAULT・NULL 許容・NOT NULL 違反の結果、(2) 列リスト省略の宣言順展開と
//! 数の不一致、(3) 同一 `OPERATION_ID`・同一文の再送が列 DEFAULT の変更を挟んでも `23505`、
//! (4) RETURNING・複数行・IDENTITY・SEQUENCE・移行済み表・ビュー・テナント境界・`$n` 型推論、
//! (5) 未対応形の fail-closed を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::params::PreparedParamType;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    exec_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn ok(core: &EngineCore, sql: &str) -> SqlOutcome {
    ok_as(core, "owner", sql)
}

fn code_as(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match exec_as(core, tenant, sql) {
        Ok(o) => panic!("statement must fail: {sql} -> {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    code_as(core, "owner", sql)
}

/// `SELECT <列> FROM <表> ORDER BY <キー>` の全行（各行は列順のセル）。
fn rows_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    core.execute_sql(&ctx(tenant), &format!("{sql} LIMIT 100"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
        .into_iter()
        .map(|r| r.cells)
        .collect()
}

fn rows(core: &EngineCore, sql: &str) -> Vec<Vec<Cell>> {
    rows_as(core, "owner", sql)
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn int(v: i64) -> Cell {
    Cell::SignedInteger(v)
}

/// 疑似列 `id`（`u64`）のセル。
fn pid(v: u64) -> Cell {
    Cell::Integer(v)
}

/// RETURNING の行（各行は列順のセル）。
fn returning_rows(out: &SqlOutcome) -> Vec<Vec<Cell>> {
    match out {
        SqlOutcome::Returning(r) => r.result.rows.iter().map(|row| row.cells.clone()).collect(),
        other => panic!("expected RETURNING, got {other:?}"),
    }
}

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let core = open_core(&path);
    ok(
        &core,
        "CREATE TABLE t (a TEXT, c TEXT DEFAULT 'dflt', n INTEGER DEFAULT 7, o TEXT)",
    );
    (core, CleanupGuard(path))
}

fn t_row(core: &EngineCore) -> Vec<Vec<Cell>> {
    rows(core, "SELECT a, c, n, o FROM t ORDER BY n")
}

/// 3 形（DEFAULT VALUES・値位置の DEFAULT・列リスト省略）で列 DEFAULT・NULL 許容列が PG と同じ結果になる。
#[test]
fn three_forms_apply_column_defaults() {
    let (core, _g) = setup("sql50-forms");
    ok(&core, "INSERT INTO t DEFAULT VALUES");
    assert_eq!(
        t_row(&core),
        vec![vec![Cell::Null, text("dflt"), int(7), Cell::Null]]
    );
    ok(&core, "DELETE FROM t WHERE n = 7 USING OPERATION_ID 'd1'");

    ok(&core, "INSERT INTO t (a, c) VALUES ('x', DEFAULT)");
    assert_eq!(
        t_row(&core),
        vec![vec![text("x"), text("dflt"), int(7), Cell::Null]]
    );
    ok(&core, "DELETE FROM t WHERE n = 7 USING OPERATION_ID 'd2'");

    ok(&core, "INSERT INTO t VALUES ('y', DEFAULT, 3, DEFAULT)");
    assert_eq!(
        t_row(&core),
        vec![vec![text("y"), text("dflt"), int(3), Cell::Null]]
    );
}

/// 値位置の `DEFAULT` は大文字小文字を問わず、明示 `NULL` には DEFAULT を適用しない（TABLE-16）。
#[test]
fn default_keyword_is_case_insensitive_and_explicit_null_stays_null() {
    let (core, _g) = setup("sql50-null");
    ok(&core, "INSERT INTO t (a, c, n) VALUES ('p', dEfAuLt, NULL)");
    ok(&core, "INSERT INTO t (a, c, n) VALUES ('q', NULL, DEFAULT)");
    assert_eq!(
        rows(&core, "SELECT a, c, n FROM t ORDER BY a"),
        vec![
            vec![text("p"), text("dflt"), Cell::Null],
            vec![text("q"), Cell::Null, int(7)],
        ]
    );
}

/// DEFAULT なしの NOT NULL 列は 3 形とも `23502`。
#[test]
fn not_null_without_default_is_rejected_in_all_forms() {
    let (core, _g) = setup("sql50-notnull");
    ok(&core, "CREATE TABLE nn (a TEXT NOT NULL, b TEXT)");
    assert_eq!(code(&core, "INSERT INTO nn DEFAULT VALUES"), "23502");
    assert_eq!(code(&core, "INSERT INTO nn (a) VALUES (DEFAULT)"), "23502");
    assert_eq!(code(&core, "INSERT INTO nn VALUES (DEFAULT, 'b')"), "23502");
    // 先頭 N 列だけ与えた残りは省略列（b は NULL 許容）。
    ok(&core, "INSERT INTO nn VALUES ('a')");
    assert_eq!(
        rows(&core, "SELECT a, b FROM nn ORDER BY a"),
        vec![vec![text("a"), Cell::Null]]
    );
}

/// 列リスト省略: 宣言順に当たり、多い値は `42601`、未知列・列数不一致（明示形）は従来どおり。
#[test]
fn column_list_omission_follows_declaration_order() {
    let (core, _g) = setup("sql50-omit");
    ok(&core, "INSERT INTO t VALUES ('a1', 'c1', 1, 'o1')");
    ok(&core, "INSERT INTO t VALUES ('a2', 'c2')");
    assert_eq!(
        rows(&core, "SELECT a, c, n, o FROM t ORDER BY a"),
        vec![
            vec![text("a1"), text("c1"), int(1), text("o1")],
            vec![text("a2"), text("c2"), int(7), Cell::Null],
        ]
    );
    assert_eq!(
        code(&core, "INSERT INTO t VALUES ('a', 'c', 1, 'o', 'x')"),
        "42601"
    );
    assert_eq!(code(&core, "INSERT INTO t (zz) VALUES (DEFAULT)"), "42703");
    assert_eq!(code(&core, "INSERT INTO t (a, c) VALUES ('x')"), "42601");
    assert_eq!(code(&core, "INSERT INTO t (a) VALUES ('x', 'y')"), "42601");
}

/// 複数行: 列リスト省略・DEFAULT を含む行。行の長さが揃わない列リスト省略は `42601`。
#[test]
fn multi_row_forms() {
    let (core, _g) = setup("sql50-multi");
    ok(
        &core,
        "INSERT INTO t VALUES ('m1', DEFAULT, 1, NULL), ('m2', 'c', DEFAULT, 'o')",
    );
    assert_eq!(
        rows(&core, "SELECT a, c, n, o FROM t ORDER BY a"),
        vec![
            vec![text("m1"), text("dflt"), int(1), Cell::Null],
            vec![text("m2"), text("c"), int(7), text("o")],
        ]
    );
    assert_eq!(
        code(&core, "INSERT INTO t VALUES ('x', 'y'), ('z')"),
        "42601"
    );
    assert_eq!(
        code(&core, "INSERT INTO t (a, c) VALUES ('x', 'y'), ('z')"),
        "42601"
    );
}

/// 再送（RECOVER-10・RECOVER-11）: 3 形とも同一 OPERATION_ID・同一文は `23505` で再実行せず、
/// 列 DEFAULT の変更・削除・追加を挟んでも同じ。内容違いは `22023`。
#[test]
fn resend_is_duplicate_even_after_default_changes() {
    let (core, _g) = setup("sql50-resend");
    let forms = [
        "INSERT INTO t DEFAULT VALUES USING OPERATION_ID 'op-dv'",
        "INSERT INTO t (a, c) VALUES ('x', DEFAULT) USING OPERATION_ID 'op-kw'",
        "INSERT INTO t VALUES ('y', DEFAULT, DEFAULT, DEFAULT) USING OPERATION_ID 'op-om'",
    ];
    for sql in forms {
        ok(&core, sql);
    }
    assert_eq!(rows(&core, "SELECT a FROM t ORDER BY n").len(), 3);
    for sql in forms {
        assert_eq!(code(&core, sql), "23505", "{sql}");
    }
    ok(&core, "ALTER TABLE t ALTER COLUMN c SET DEFAULT 'changed'");
    for sql in forms {
        assert_eq!(code(&core, sql), "23505", "{sql}");
    }
    ok(&core, "ALTER TABLE t ALTER COLUMN c DROP DEFAULT");
    ok(&core, "ALTER TABLE t ADD COLUMN e TEXT DEFAULT 'e'");
    for sql in forms {
        assert_eq!(code(&core, sql), "23505", "{sql}");
    }
    assert_eq!(rows(&core, "SELECT a FROM t ORDER BY n").len(), 3);
    // 同一 OPERATION_ID で内容が違う文は `22023`。
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (a, c) VALUES ('other', DEFAULT) USING OPERATION_ID 'op-kw'"
        ),
        "22023"
    );
}

/// RETURNING は 3 形でも従来と同じ列・行を DEFAULT 解決後の値で返す。
#[test]
fn returning_reports_resolved_defaults() {
    let (core, _g) = setup("sql50-returning");
    let out = ok(&core, "INSERT INTO t DEFAULT VALUES RETURNING a, c, n, o");
    assert_eq!(
        returning_rows(&out),
        vec![vec![Cell::Null, text("dflt"), int(7), Cell::Null]]
    );
    let out = ok(
        &core,
        "INSERT INTO t (a, n) VALUES ('k', DEFAULT) RETURNING a, n",
    );
    assert_eq!(returning_rows(&out), vec![vec![text("k"), int(7)]]);
    let out = ok(&core, "INSERT INTO t VALUES ('z', 'cz') RETURNING *");
    assert_eq!(
        returning_rows(&out),
        vec![vec![text("z"), text("cz"), int(7), Cell::Null]]
    );
}

/// IDENTITY・SEQUENCE の DEFAULT は 3 形で採番され、再送は採番を進めない。GENERATED ALWAYS 列にも
/// `DEFAULT` は指定でき、明示値は従来どおり `428C9`。
#[test]
fn identity_and_sequence_defaults_are_assigned_once() {
    let (core, _g) = setup("sql50-seq");
    ok(&core, "CREATE SEQUENCE sq");
    ok(
        &core,
        "CREATE TABLE s (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, q BIGINT DEFAULT nextval('sq'), v TEXT)",
    );
    ok(
        &core,
        "INSERT INTO s DEFAULT VALUES USING OPERATION_ID 's1'",
    );
    ok(&core, "INSERT INTO s (id, v) VALUES (DEFAULT, 'b')");
    ok(&core, "INSERT INTO s VALUES (DEFAULT, DEFAULT, 'c')");
    assert_eq!(
        code(
            &core,
            "INSERT INTO s DEFAULT VALUES USING OPERATION_ID 's1'"
        ),
        "23505"
    );
    assert_eq!(
        rows(&core, "SELECT id, q, v FROM s ORDER BY id"),
        vec![
            vec![int(1), int(1), Cell::Null],
            vec![int(2), int(2), text("b")],
            vec![int(3), int(3), text("c")],
        ]
    );
    assert_eq!(
        code(&core, "INSERT INTO s (id, v) VALUES (9, 'x')"),
        "428C9"
    );
    ok(&core, "INSERT INTO s DEFAULT VALUES");
    assert_eq!(rows(&core, "SELECT id FROM s ORDER BY id").len(), 4);
}

/// 構文の fail-closed: 式の中の DEFAULT・引用付き "DEFAULT"・余剰形・OVERRIDING・UPDATE の
/// `SET c = DEFAULT` は受理しない。
#[test]
fn unsupported_shapes_stay_rejected() {
    let (core, _g) = setup("sql50-reject");
    ok(
        &core,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, c TEXT DEFAULT 'd')",
    );
    ok(&core, "INSERT INTO u (id) VALUES (1)");
    for sql in [
        "INSERT INTO t (n) VALUES (DEFAULT + 1)",
        "INSERT INTO t (n) VALUES ((DEFAULT))",
        "INSERT INTO t (a) VALUES (\"DEFAULT\")",
        "INSERT INTO t DEFAULT VALUES (1)",
        "INSERT INTO t (a) DEFAULT VALUES",
        "INSERT INTO t DEFAULT",
        "INSERT INTO t DEFAULT VALUES, DEFAULT VALUES",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    // UPDATE の `SET c = DEFAULT` は従来どおり式（列参照）として解釈され、存在しない列 `DEFAULT` で拒否される
    // （`Default` variant は UPDATE へ到達しない。UPDATE の DEFAULT 対応は後続）。
    assert_eq!(
        code(
            &core,
            "UPDATE u SET c = DEFAULT WHERE id = 1 USING OPERATION_ID 'u'"
        ),
        "42703"
    );
    for sql in [
        "INSERT INTO t OVERRIDING SYSTEM VALUE VALUES ('x')",
        "INSERT INTO t (a) OVERRIDING USER VALUE VALUES ('x')",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (a) VALUES ('x') ON CONFLICT (a) DO UPDATE SET c = DEFAULT"
        ),
        "42601"
    );
    assert!(rows(&core, "SELECT a FROM t ORDER BY n").is_empty());
}

/// ビュー経由: `DEFAULT VALUES`・DEFAULT キーワードは基表の DEFAULT になり、列リスト省略は `0A000`。
#[test]
fn view_insert_uses_base_defaults_and_rejects_omitted_list() {
    let (core, _g) = setup("sql50-view");
    ok(&core, "CREATE VIEW v AS SELECT a, c, n FROM t");
    ok(&core, "INSERT INTO v DEFAULT VALUES");
    ok(&core, "INSERT INTO v (a, c) VALUES ('vv', DEFAULT)");
    assert_eq!(
        rows(&core, "SELECT a, c, n FROM t ORDER BY a"),
        vec![
            vec![text("vv"), text("dflt"), int(7)],
            vec![Cell::Null, text("dflt"), int(7)],
        ]
    );
    assert_eq!(code(&core, "INSERT INTO v VALUES ('x', 'y', 1)"), "0A000");
}

/// テナント境界: DEFAULT 補完行はセッションのテナントが所有し、他テナントからは見えない。
#[test]
fn defaults_respect_tenant_boundary() {
    let (core, _g) = setup("sql50-tenant");
    ok_as(&core, "tenant-a", "INSERT INTO t DEFAULT VALUES");
    ok_as(&core, "tenant-a", "INSERT INTO t VALUES ('a-only')");
    ok_as(&core, "tenant-b", "INSERT INTO t (a) VALUES (DEFAULT)");
    assert_eq!(
        rows_as(&core, "tenant-a", "SELECT a FROM t ORDER BY a").len(),
        2
    );
    assert_eq!(
        rows_as(&core, "tenant-b", "SELECT a, c FROM t ORDER BY a"),
        vec![vec![Cell::Null, text("dflt")]]
    );
    assert!(rows_as(&core, "tenant-c", "SELECT a FROM t ORDER BY a").is_empty());
}

/// 拡張クエリ: 列リスト省略の `$n` は表の宣言順の列型で推論され、`DEFAULT VALUES` の
/// `USING OPERATION_ID $1` を受理する。
#[test]
fn extended_query_param_types_and_operation_id() {
    let (core, _g) = setup("sql50-ext");
    let prepared = core
        .parse_sql_prepared("INSERT INTO t VALUES ($1, $2, $3, DEFAULT) USING OPERATION_ID 'p-1'")
        .expect("prepare");
    let types = prepared.param_types();
    assert_eq!(types.len(), 3);
    assert!(
        matches!(types.get(2), Some(PreparedParamType::Column(_))),
        "third parameter must be typed from the INTEGER column: {types:?}"
    );
    let bound = core
        .bind_prepared(
            &prepared,
            &[
                Some(b"x".to_vec()),
                Some(b"y".to_vec()),
                Some(b"5".to_vec()),
            ],
        )
        .expect("bind");
    let _ = bound;

    let prepared = core
        .parse_sql_prepared("INSERT INTO t DEFAULT VALUES USING OPERATION_ID $1")
        .expect("prepare DEFAULT VALUES");
    assert_eq!(prepared.param_types().len(), 1);
}

/// 移行済み表（疑似 `id` あり）: `DEFAULT VALUES`・`id = DEFAULT` は IDENTITY 採番され、
/// ビュー経由の `id = DEFAULT` は D9 と同じく `0A000`。
#[test]
fn migrated_id_table_assigns_identity_for_default_forms() {
    let path = unique_db_path("sql50-migrated");
    let _g = CleanupGuard(path.clone());
    {
        let core = EngineCore::open(&path).expect("open");
        let mut s = SessionState::default();
        s.allow_ddl();
        core.execute_sql_in_session_legacy(&ctx("sys"), &mut s, "CREATE TABLE items (n BIGINT)")
            .expect("create");
        let mut s = SessionState::default();
        core.execute_sql_in_session_legacy(
            &ctx("owner"),
            &mut s,
            "INSERT INTO items (id, n) VALUES (3, 3)",
        )
        .expect("seed");
    }
    let core = EngineCore::open(&path).expect("reopen migrates");
    ok(&core, "INSERT INTO items DEFAULT VALUES");
    ok(&core, "INSERT INTO items (id, n) VALUES (DEFAULT, 9)");
    ok(&core, "INSERT INTO items VALUES (DEFAULT, 10)");
    assert_eq!(
        rows(&core, "SELECT id, n FROM items ORDER BY id"),
        vec![
            vec![pid(3), int(3)],
            vec![pid(4), Cell::Null],
            vec![pid(5), int(9)],
            vec![pid(6), int(10)],
        ]
    );
    // 明示値と DEFAULT の混在は既存どおり `0A000`。
    assert_eq!(
        code(
            &core,
            "INSERT INTO items (id, n) VALUES (20, 1), (DEFAULT, 2)"
        ),
        "0A000"
    );
    ok(&core, "CREATE VIEW iv AS SELECT id, n FROM items");
    assert_eq!(
        code(&core, "INSERT INTO iv (id, n) VALUES (DEFAULT, 1)"),
        "0A000"
    );
    assert_eq!(code(&core, "INSERT INTO iv DEFAULT VALUES"), "0A000");
}
