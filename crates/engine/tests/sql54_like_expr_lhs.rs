//! `WHERE <式> [NOT] LIKE|ILIKE '<pattern>'`（式左辺の LIKE）の結合テスト（Issue #1782）。
//!
//! ポインタ: SQL-54 (3)(5)（`docs/spec/04-behavior/` 配下。本文は転記しない）。
//! `EngineCore::execute_sql_in_session` の本番経路を実 `Storage` 越しに通し、一致・不一致・
//! 否定・`ILIKE`・`ESCAPE`・NULL 伝播（三値論理）・型エラー（`42883`）・パターンエラー・
//! 照合の拒否・索引非使用の正しさ・テナント境界・述語形 `UPDATE`／`DELETE` の内容照合・
//! fail-closed の拒否形を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl(core: &EngineCore, sql: &str) -> Result<(), String> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .map(|_| ())
        .map_err(|e| e.wire_code().to_string())
}

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    ddl(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, s TEXT, k TEXT, n INTEGER, \
         iu TEXT COLLATE \"und-x-icu\")",
    )
    .expect("create table");
    (core, CleanupGuard(path))
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql)
        .map_err(|e| e.wire_code().to_string())
}

fn insert(core: &EngineCore, tenant: &str, id: u64, s: Option<&str>, k: &str) {
    let sv = s.map_or("NULL".to_string(), |v| format!("'{v}'"));
    let sql = format!(
        "INSERT INTO t (id, s, k, n, iu) VALUES ({id}, {sv}, '{k}', {id}, {sv}) \
         USING OPERATION_ID '{tenant}-ins-{id}'"
    );
    exec(core, tenant, &sql).unwrap_or_else(|e| panic!("insert {sql:?} failed: {e}"));
}

/// id 1..=6（id 5 は `s` が NULL）。
fn seed(core: &EngineCore) {
    insert(core, "tenant-a", 1, Some("Src/Lib"), "a");
    insert(core, "tenant-a", 2, Some("src/main"), "b");
    insert(core, "tenant-a", 3, Some("docs/readme"), "a");
    insert(core, "tenant-a", 4, Some("ABC"), "b");
    insert(core, "tenant-a", 5, None, "a");
    insert(core, "tenant-a", 6, Some("100%"), "b");
}

fn ids_of(core: &EngineCore, tenant: &str, where_clause: &str) -> Result<Vec<u64>, String> {
    let sql = format!("SELECT id FROM t WHERE {where_clause} LIMIT 100");
    match exec(core, tenant, &sql)? {
        SqlOutcome::Query(r) => {
            let mut v: Vec<u64> = r
                .rows
                .iter()
                .map(|row| match row.cells.first() {
                    Some(Cell::Integer(n)) => *n,
                    Some(Cell::SignedInteger(n)) => u64::try_from(*n).expect("non-negative id"),
                    other => panic!("expected id cell, got {other:?}"),
                })
                .collect();
            v.sort_unstable();
            Ok(v)
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

fn ids(core: &EngineCore, where_clause: &str) -> Vec<u64> {
    ids_of(core, "tenant-a", where_clause)
        .unwrap_or_else(|e| panic!("{where_clause:?} should succeed, got {e}"))
}

fn err_code(core: &EngineCore, where_clause: &str) -> String {
    ids_of(core, "tenant-a", where_clause)
        .expect_err(&format!("{where_clause:?} should be rejected"))
}

/// 一致・不一致（関数呼び出し・`concat`・`CASE` の左辺）。
#[test]
fn matches_and_mismatches() {
    let (core, _g) = setup("sql54-like-expr-match");
    seed(&core);
    assert_eq!(ids(&core, "lower(s) LIKE 'src/%'"), vec![1, 2]);
    assert_eq!(ids(&core, "s LIKE 'src/%'"), vec![2]);
    assert_eq!(
        ids(&core, "concat(s, 'x') LIKE '%x'"),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert_eq!(ids(&core, "concat(s, 'x') LIKE 'abc'"), Vec::<u64>::new());
    assert_eq!(ids(&core, "concat(lower(s), '!') LIKE 'abc!'"), vec![4]);
    assert_eq!(
        ids(&core, "CASE WHEN k = 'a' THEN s ELSE k END LIKE 'docs%'"),
        vec![3]
    );
    assert_eq!(
        ids(&core, "CASE WHEN k = 'a' THEN s ELSE k END LIKE 'b'"),
        vec![2, 4, 6]
    );
    assert_eq!(ids(&core, "upper(s) LIKE 'ABC'"), vec![4]);
}

/// 否定（後置・前置・De Morgan）・`ILIKE`・`ESCAPE`・括弧付き左辺。
#[test]
fn negation_ilike_escape_and_grouping() {
    let (core, _g) = setup("sql54-like-expr-neg");
    seed(&core);
    assert_eq!(ids(&core, "lower(s) NOT LIKE 'src/%'"), vec![3, 4, 6]);
    assert_eq!(ids(&core, "NOT lower(s) LIKE 'src/%'"), vec![3, 4, 6]);
    assert_eq!(ids(&core, "NOT NOT lower(s) LIKE 'src/%'"), vec![1, 2]);
    assert_eq!(
        ids(&core, "NOT (lower(s) LIKE 'src/%' AND k = 'a')"),
        vec![2, 3, 4, 6]
    );
    assert_eq!(ids(&core, "trim(s) ILIKE 'SRC/%'"), vec![1, 2]);
    assert_eq!(ids(&core, "trim(s) NOT ILIKE 'SRC/%'"), vec![3, 4, 6]);
    assert_eq!(ids(&core, "lower(s) LIKE '100!%' ESCAPE '!'"), vec![6]);
    assert_eq!(ids(&core, "lower(s) LIKE '100\\%'"), vec![6]);
    assert_eq!(ids(&core, "(lower(s)) LIKE 'src/%'"), vec![1, 2]);
    assert_eq!(ids(&core, "(concat(s, 'x')) LIKE '%/Libx'"), vec![1]);
    assert_eq!(ids(&core, "NOT (lower(s)) LIKE 'src/%'"), vec![3, 4, 6]);
}

/// NULL 伝播: 左辺が NULL の行は LIKE・NOT LIKE・NOT (...) のいずれでも除外される（三値論理）。
#[test]
fn null_lhs_is_never_selected() {
    let (core, _g) = setup("sql54-like-expr-null");
    seed(&core);
    assert_eq!(ids(&core, "lower(s) LIKE '%'"), vec![1, 2, 3, 4, 6]);
    assert_eq!(ids(&core, "lower(s) NOT LIKE 'zzz'"), vec![1, 2, 3, 4, 6]);
    assert_eq!(ids(&core, "NOT (lower(s) LIKE 'zzz')"), vec![1, 2, 3, 4, 6]);
    assert_eq!(ids(&core, "NOT lower(s) LIKE 'zzz'"), vec![1, 2, 3, 4, 6]);
    assert_eq!(ids(&core, "lower(s) NOT ILIKE '%'"), Vec::<u64>::new());
    // COALESCE で NULL を空文字へ寄せれば行は対象になる。
    assert_eq!(ids(&core, "coalesce(s, '') LIKE ''"), vec![5]);
}

/// 型エラー: 左辺が文字列型でなければ `42883`（`42804` ではない）。
#[test]
fn non_text_lhs_is_undefined_function() {
    let (core, _g) = setup("sql54-like-expr-type");
    seed(&core);
    assert_eq!(err_code(&core, "(n + 1) LIKE '1%'"), "42883");
    assert_eq!(err_code(&core, "abs(n) NOT LIKE '1%'"), "42883");
    assert_eq!(err_code(&core, "abs(n) ILIKE '1%'"), "42883");
    assert_eq!(
        err_code(&core, "CASE WHEN n > 1 THEN 1 ELSE 2 END LIKE '1'"),
        "42883"
    );
}

/// パターンのエラー: 末尾の孤立エスケープは `22025`、長さ上限超過は `54000`。
#[test]
fn pattern_errors() {
    let (core, _g) = setup("sql54-like-expr-pat");
    seed(&core);
    assert_eq!(err_code(&core, "lower(s) LIKE 'abc\\'"), "22025");
    let over = "a".repeat(4097);
    assert_eq!(err_code(&core, &format!("lower(s) LIKE '{over}'")), "54000");
}

/// 照合: ICU 列・非バイト列順の `COLLATE` を式の内側で使う形は `0A000`、`"C"` は受理。
#[test]
fn collation_rules() {
    let (core, _g) = setup("sql54-like-expr-coll");
    seed(&core);
    assert_eq!(err_code(&core, "lower(iu) ILIKE 'src%'"), "0A000");
    assert_eq!(
        err_code(&core, "concat(s COLLATE \"und-x-icu\", 'x') LIKE 'src%'"),
        "0A000"
    );
    assert_eq!(
        ids(&core, "concat(s COLLATE \"C\", 'x') LIKE 'src/%'"),
        vec![2]
    );
}

/// 索引: 二次索引のある列でも式左辺は全行評価され、索引を誤用すると取りこぼす行も含めて正しい。
#[test]
fn secondary_index_does_not_hide_rows() {
    let (core, _g) = setup("sql54-like-expr-index");
    ddl(&core, "CREATE INDEX ix_s ON t (s)").expect("create index");
    seed(&core);
    assert_eq!(ids(&core, "s LIKE 'src/%'"), vec![2]);
    assert_eq!(ids(&core, "lower(s) LIKE 'src/%'"), vec![1, 2]);
    assert_eq!(ids(&core, "lower(s) LIKE 'src/%' AND k = 'a'"), vec![1]);
}

/// テナント境界: 他テナントの行は結果にも現れない。
#[test]
fn tenant_boundary() {
    let (core, _g) = setup("sql54-like-expr-tenant");
    seed(&core);
    insert(&core, "tenant-b", 100, Some("src/other"), "a");
    assert_eq!(ids(&core, "lower(s) LIKE 'src/%'"), vec![1, 2]);
    assert_eq!(
        ids_of(&core, "tenant-b", "lower(s) LIKE 'src/%'").expect("tenant-b select"),
        vec![100]
    );
}

/// 述語形 `UPDATE`／`DELETE`: 対象の正しさと、同一 operation_id の再送冪等・内容違いの検出。
#[test]
fn predicate_update_and_delete() {
    let (core, _g) = setup("sql54-like-expr-dml");
    seed(&core);
    let upd = "UPDATE t SET k = 'hit' WHERE lower(s) LIKE 'src/%' USING OPERATION_ID 'u-1'";
    exec(&core, "tenant-a", upd).expect("update");
    assert_eq!(ids(&core, "k = 'hit'"), vec![1, 2]);
    assert_eq!(exec(&core, "tenant-a", upd).expect_err("replay"), "23505");
    let changed_expr =
        "UPDATE t SET k = 'hit' WHERE upper(s) LIKE 'src/%' USING OPERATION_ID 'u-1'";
    assert_eq!(
        exec(&core, "tenant-a", changed_expr).expect_err("changed lhs"),
        "22023"
    );
    let changed_kind =
        "UPDATE t SET k = 'hit' WHERE lower(s) ILIKE 'src/%' USING OPERATION_ID 'u-1'";
    assert_eq!(
        exec(&core, "tenant-a", changed_kind).expect_err("changed kind"),
        "22023"
    );
    exec(
        &core,
        "tenant-a",
        "DELETE FROM t WHERE lower(s) NOT LIKE 'src/%' USING OPERATION_ID 'd-1'",
    )
    .expect("delete");
    assert_eq!(ids(&core, "id > 0"), vec![1, 2, 5]);
}

/// 未対応の形は fail-closed（CHECK・部分索引・ビュー・パターンが式／列）。
#[test]
fn unsupported_forms_fail_closed() {
    let (core, _g) = setup("sql54-like-expr-closed");
    seed(&core);
    assert_eq!(
        ddl(&core, "CREATE INDEX ix_p ON t (k) WHERE lower(s) LIKE 'a%'").expect_err("partial"),
        "0A000"
    );
    assert_eq!(
        ddl(
            &core,
            "ALTER TABLE t ADD CONSTRAINT c1 CHECK (lower(s) LIKE 'a%')"
        )
        .expect_err("check"),
        "0A000"
    );
    assert_eq!(
        ddl(
            &core,
            "CREATE VIEW v AS SELECT id FROM t WHERE lower(s) LIKE 'a%'"
        )
        .expect_err("view"),
        "42601"
    );
    assert_eq!(err_code(&core, "lower(s) LIKE k"), "42601");
    assert_eq!(err_code(&core, "lower(s) LIKE 'a' || 'b'"), "42601");
}
