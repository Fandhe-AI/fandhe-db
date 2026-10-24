//! `WHERE <式> ~ | ~* | !~ | !~* '<pattern>'`（正規表現マッチ演算子）の結合テスト（Issue #1787）。
//!
//! ポインタ: SQL-55・ERR-2（`docs/spec/04-behavior/` 配下。本文は転記しない）。
//! `EngineCore::execute_sql_in_session` の本番経路を実 `Storage` 越しに通し、4 演算子の
//! 一致・不一致・NULL 伝播（三値論理）・不正／非対応／上限超過パターンの SQLSTATE・
//! 上限の運搬（`with_regex_limits`）・型／照合の拒否・述語形 `UPDATE`／`DELETE` の
//! operation_id 再送・fail-closed の拒否形を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::regex_limits::RegexLimits;
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

fn build(label: &str, limits: Option<RegexLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(l) = limits {
        core = core.with_regex_limits(l);
    }
    ddl(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, s TEXT, k TEXT, n INTEGER, \
         iu TEXT COLLATE \"und-x-icu\")",
    )
    .expect("create table");
    (core, CleanupGuard(path))
}

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    build(label, None)
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql)
        .map_err(|e| e.wire_code().to_string())
}

fn insert(core: &EngineCore, id: u64, s: Option<&str>, k: &str) {
    let sv = s.map_or("NULL".to_string(), |v| format!("'{v}'"));
    let sql = format!(
        "INSERT INTO t (id, s, k, n, iu) VALUES ({id}, {sv}, '{k}', {id}, {sv}) \
         USING OPERATION_ID 'ins-{id}'"
    );
    exec(core, "tenant-a", &sql).unwrap_or_else(|e| panic!("insert {sql:?} failed: {e}"));
}

/// id 1..=6（id 5 は `s` が NULL）。
fn seed(core: &EngineCore) {
    insert(core, 1, Some("Src/Lib"), "a");
    insert(core, 2, Some("src/main"), "b");
    insert(core, 3, Some("docs/readme"), "a");
    insert(core, 4, Some("ABC"), "b");
    insert(core, 5, None, "a");
    insert(core, 6, Some("é1"), "b");
}

fn ids_of(core: &EngineCore, where_clause: &str) -> Result<Vec<u64>, String> {
    let sql = format!("SELECT id FROM t WHERE {where_clause} LIMIT 100");
    match exec(core, "tenant-a", &sql)? {
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
    ids_of(core, where_clause)
        .unwrap_or_else(|e| panic!("{where_clause:?} should succeed, got {e}"))
}

fn err_code(core: &EngineCore, where_clause: &str) -> String {
    ids_of(core, where_clause).expect_err(&format!("{where_clause:?} should be rejected"))
}

/// 4 演算子の一致・不一致。`~` は部分一致で、`^`／`$` でアンカーできる。
#[test]
fn four_operators_match_and_mismatch() {
    let (core, _g) = setup("sql55-ops");
    seed(&core);
    assert_eq!(ids(&core, "s ~ 'src'"), vec![2]);
    assert_eq!(ids(&core, "s ~* 'src'"), vec![1, 2]);
    assert_eq!(ids(&core, "s !~ 'src'"), vec![1, 3, 4, 6]);
    assert_eq!(ids(&core, "s !~* 'src'"), vec![3, 4, 6]);
    assert_eq!(ids(&core, "s ~ '^docs/'"), vec![3]);
    assert_eq!(ids(&core, "s ~ 'lib$'"), Vec::<u64>::new());
    assert_eq!(ids(&core, "s ~* 'lib$'"), vec![1]);
    assert_eq!(ids(&core, "s ~ '^(src|docs)/[a-z]+$'"), vec![2, 3]);
    assert_eq!(ids(&core, "s ~ ''"), vec![1, 2, 3, 4, 6]);
}

/// `~*` は ASCII だけを区別しない（非 ASCII の大小は畳み込まない）。
#[test]
fn case_insensitive_is_ascii_only() {
    let (core, _g) = setup("sql55-ascii");
    seed(&core);
    assert_eq!(ids(&core, "s ~* 'abc'"), vec![4]);
    assert_eq!(ids(&core, "s ~* '^É'"), Vec::<u64>::new());
    assert_eq!(ids(&core, "s ~* '^é'"), vec![6]);
}

/// 左辺 NULL の行は 4 演算子とも、前置 `NOT` を付けても返らない。パターン `NULL` は 0 件。
#[test]
fn null_semantics_follow_three_valued_logic() {
    let (core, _g) = setup("sql55-null");
    seed(&core);
    for op in ["~", "~*", "!~", "!~*"] {
        let got = ids(&core, &format!("s {op} 'x'"));
        assert!(!got.contains(&5), "{op}: NULL lhs row must not match");
        let got = ids(&core, &format!("NOT (s {op} 'x')"));
        assert!(!got.contains(&5), "NOT {op}: NULL lhs row must not match");
        assert_eq!(ids(&core, &format!("s {op} NULL")), Vec::<u64>::new());
        assert_eq!(ids(&core, &format!("NOT (s {op} NULL)")), Vec::<u64>::new());
    }
    // 否定は補集合（NULL 行を除く）。
    assert_eq!(ids(&core, "NOT (s ~ 'src')"), vec![1, 3, 4, 6]);
    assert_eq!(ids(&core, "NOT (s !~ 'src')"), vec![2]);
    assert_eq!(ids(&core, "NOT (s ~ 'src' OR s ~ 'docs')"), vec![1, 4, 6]);
}

/// 式左辺・括弧付き左辺・AND／OR との組み合わせ。
#[test]
fn expression_lhs_and_combinations() {
    let (core, _g) = setup("sql55-expr");
    seed(&core);
    assert_eq!(ids(&core, "lower(s) ~ '^src'"), vec![1, 2]);
    assert_eq!(ids(&core, "(lower(s)) ~ '^src' AND id = 1"), vec![1]);
    assert_eq!(ids(&core, "(s) ~ 'docs' OR k = 'b'"), vec![2, 3, 4, 6]);
    assert_eq!(ids(&core, "concat(s, '!') ~ 'C!$'"), vec![4]);
    assert_eq!(ids(&core, "s ~ 'src' AND k = 'b'"), vec![2]);
}

/// 不正パターンは `2201B`、非対応構文は `0A000`、上限超過は `54000`。
#[test]
fn pattern_errors_map_to_sqlstate() {
    let (core, _g) = setup("sql55-errors");
    seed(&core);
    assert_eq!(err_code(&core, "s ~ '('"), "2201B");
    assert_eq!(err_code(&core, "s ~* 'a{2,1}'"), "2201B");
    assert_eq!(err_code(&core, "s !~ '['"), "2201B");
    assert_eq!(err_code(&core, "s ~ '(a)\\1'"), "0A000");
    assert_eq!(err_code(&core, "s !~* '(?=a)'"), "0A000");
    let long = "a".repeat(4097);
    assert_eq!(err_code(&core, &format!("s ~ '{long}'")), "54000");
    // 行が 0 件でも束縛時に拒否する（fail-closed）。
    assert_eq!(err_code(&core, "id < 0 AND s ~ '('"), "2201B");
}

/// `with_regex_limits` で下げた上限が束縛・評価へ運ばれる（境界ちょうどは受理）。
#[test]
fn configured_limits_apply() {
    let (core, _g) = build("sql55-limits", Some(RegexLimits::new(5, 8, 1024 * 1024)));
    insert(&core, 1, Some("abcdefgh"), "a");
    insert(&core, 2, Some("abcdefghi"), "a");
    assert_eq!(err_code(&core, "s ~ 'abcdef'"), "54000");
    // パターン長 5 は受理。入力長 9 の行は評価時に 54000。
    assert_eq!(err_code(&core, "s ~ 'abcde'"), "54000");
    assert_eq!(ids(&core, "id = 1 AND s ~ 'abcde'"), vec![1]);
}

/// 型・照合: 非 TEXT 左辺は `42883`、ICU 照合は `0A000`、`COLLATE "C"` は受理。
#[test]
fn type_and_collation_rules() {
    let (core, _g) = setup("sql55-types");
    seed(&core);
    assert_eq!(err_code(&core, "n ~ '1'"), "42883");
    assert_eq!(err_code(&core, "iu ~ 'a'"), "0A000");
    assert_eq!(err_code(&core, "s COLLATE \"und-x-icu\" ~ 'a'"), "0A000");
    assert_eq!(ids(&core, "s COLLATE \"C\" ~ '^src'"), vec![2]);
}

/// 述語形 `UPDATE`／`DELETE`: 対象の正しさと、同一 operation_id の再送冪等・内容違いの検出。
#[test]
fn predicate_update_and_delete() {
    let (core, _g) = setup("sql55-dml");
    seed(&core);
    let upd = "UPDATE t SET k = 'hit' WHERE s ~* '^src' USING OPERATION_ID 'u-1'";
    exec(&core, "tenant-a", upd).expect("update");
    assert_eq!(ids(&core, "k = 'hit'"), vec![1, 2]);
    assert_eq!(exec(&core, "tenant-a", upd).expect_err("replay"), "23505");
    for changed in [
        "UPDATE t SET k = 'hit' WHERE s ~* '^doc' USING OPERATION_ID 'u-1'",
        "UPDATE t SET k = 'hit' WHERE s ~ '^src' USING OPERATION_ID 'u-1'",
        "UPDATE t SET k = 'hit' WHERE s !~* '^src' USING OPERATION_ID 'u-1'",
    ] {
        assert_eq!(
            exec(&core, "tenant-a", changed).expect_err("changed content"),
            "22023",
            "{changed}"
        );
    }
    exec(
        &core,
        "tenant-a",
        "DELETE FROM t WHERE s !~ '^(Src|src)' USING OPERATION_ID 'd-1'",
    )
    .expect("delete");
    // NULL 行（id 5）は `!~` でも対象外。
    assert_eq!(ids(&core, "id > 0"), vec![1, 2, 5]);
}

/// 未対応の位置は fail-closed（SELECT リスト・CHECK・索引の述語・パターンが列／式）。
#[test]
fn unsupported_forms_fail_closed() {
    let (core, _g) = setup("sql55-closed");
    seed(&core);
    assert_eq!(
        exec(&core, "tenant-a", "SELECT s ~ 'a' FROM t LIMIT 10").expect_err("projection"),
        "42601"
    );
    assert_eq!(
        ddl(&core, "CREATE INDEX ix_p ON t (k) WHERE s ~ 'a'").expect_err("partial"),
        "0A000"
    );
    assert_eq!(
        ddl(&core, "ALTER TABLE t ADD CONSTRAINT c1 CHECK (s ~ 'a')").expect_err("check"),
        "0A000"
    );
    assert_eq!(err_code(&core, "s ~ k"), "42601");
    assert_eq!(err_code(&core, "s ~ 'a'::text"), "42601");
    assert_eq!(err_code(&core, "s ~ 1"), "42601");
    assert_eq!(err_code(&core, "s ~ 'a' || 'b'"), "42601");
}

/// 一時表の経路でも、起動時に下げた上限が効く（fail-open の回帰防止）。
#[test]
fn temp_table_inherits_configured_limits() {
    let (core, _g) = build("sql55-temp", Some(RegexLimits::new(3, 64, 1024 * 1024)));
    let mut session = SessionState::default();
    session.allow_ddl();
    let c = ctx("tenant-a");
    core.execute_sql_in_session(
        &c,
        &mut session,
        "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, s TEXT)",
    )
    .expect("create temp");
    core.execute_sql_in_session(
        &c,
        &mut session,
        "INSERT INTO tt (id, s) VALUES (1, 'abcd')",
    )
    .expect("insert temp");
    let err = core
        .execute_sql_in_session(
            &c,
            &mut session,
            "SELECT id FROM tt WHERE s ~ 'abcd' LIMIT 10",
        )
        .expect_err("pattern longer than the configured limit");
    assert_eq!(err.wire_code(), "54000");
    core.execute_sql_in_session(
        &c,
        &mut session,
        "SELECT id FROM tt WHERE s ~ 'abc' LIMIT 10",
    )
    .expect("within limit");
}

/// JOIN の WHERE・HAVING・サブクエリ・CASE の条件・ビュー本体・式索引は本 Issue の対象外で、
/// 黙って誤評価せず `42601` で fail-closed になる。
#[test]
fn other_contexts_fail_closed() {
    let (core, _g) = setup("sql55-other");
    seed(&core);
    for q in [
        "SELECT a.id FROM t a JOIN t b ON a.id = b.id WHERE a.s ~ 'src' LIMIT 10",
        "SELECT k FROM t GROUP BY k HAVING k ~ 'a' LIMIT 10",
        "SELECT id FROM t WHERE id IN (SELECT id FROM t WHERE s ~ 'src') LIMIT 10",
        "SELECT CASE WHEN s ~ 'a' THEN 1 ELSE 0 END FROM t LIMIT 10",
    ] {
        assert_eq!(exec(&core, "tenant-a", q).expect_err(q), "42601", "{q}");
    }
    for q in [
        "CREATE VIEW v AS SELECT id FROM t WHERE s ~ 'a'",
        "CREATE INDEX ix ON t ((s ~ 'a'))",
    ] {
        assert_eq!(ddl(&core, q).expect_err(q), "42601", "{q}");
    }
}
