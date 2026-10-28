//! `WHERE <式> [NOT] SIMILAR TO '<pattern>' [ESCAPE '<c>']` の結合テスト（Issue #1788）。
//!
//! ポインタ: SQL-55・ERR-2・ERR-6（`docs/spec/04-behavior/` 配下。本文は転記しない）。
//! `EngineCore::execute_sql_in_session` の本番経路を実 `Storage` 越しに通し、全体一致・
//! `ESCAPE` の 3 形態・NULL 伝播（三値論理）・不正／非対応／上限超過パターンの SQLSTATE・
//! 型／照合の拒否・述語形 `UPDATE`／`DELETE` の operation_id 再送・RLS（他テナントの行が
//! 照合対象にならない）・fail-closed の拒否形を固定する。変換規則の網羅は
//! `tests/similar_to_match.rs`（PG 17 実測表）が担う。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::regex_limits::RegexLimits;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
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

/// 全体一致（`~` と違い両端アンカー）・`%`・`_`・選択・量指定・ブラケット。
#[test]
fn matches_whole_string_like_pg() {
    let (core, _g) = setup("sql55s-whole");
    seed(&core);
    assert_eq!(ids(&core, "s SIMILAR TO 'src/%'"), vec![2]);
    assert_eq!(ids(&core, "s SIMILAR TO 'Src/Lib'"), vec![1]);
    assert_eq!(ids(&core, "s SIMILAR TO 'src'"), Vec::<u64>::new());
    assert_eq!(ids(&core, "s SIMILAR TO '%rc%'"), vec![1, 2]);
    assert_eq!(ids(&core, "s SIMILAR TO '_rc/%'"), vec![1, 2]);
    assert_eq!(ids(&core, "s SIMILAR TO '(src|docs)/%'"), vec![2, 3]);
    assert_eq!(ids(&core, "s SIMILAR TO '[A-Z]%'"), vec![1, 4]);
    assert_eq!(ids(&core, "s SIMILAR TO '[^a-z]%'"), vec![1, 4, 6]);
    assert_eq!(ids(&core, "s SIMILAR TO '(src/)+%'"), vec![2]);
    assert_eq!(ids(&core, "s SIMILAR TO '___'"), vec![4]);
    assert_eq!(ids(&core, "s SIMILAR TO 'é_'"), vec![6]);
    assert_eq!(ids(&core, "s SIMILAR TO '[a-z]{3}/[a-z]{4}'"), vec![2]);
    assert_eq!(ids(&core, "s SIMILAR TO ''"), Vec::<u64>::new());
    // 正規表現のメタ文字 `.`・`^`・`$` は SQL 正規表現ではリテラル。
    assert_eq!(ids(&core, "s SIMILAR TO 'src.main'"), Vec::<u64>::new());
    assert_eq!(ids(&core, "s SIMILAR TO '%$'"), Vec::<u64>::new());
}

/// `NOT SIMILAR TO`・前置 `NOT`。左辺 NULL の行は返らない。
#[test]
fn negation_forms() {
    let (core, _g) = setup("sql55s-neg");
    seed(&core);
    assert_eq!(ids(&core, "s NOT SIMILAR TO 'src/%'"), vec![1, 3, 4, 6]);
    assert_eq!(ids(&core, "NOT (s SIMILAR TO 'src/%')"), vec![1, 3, 4, 6]);
    assert_eq!(ids(&core, "NOT (s NOT SIMILAR TO 'src/%')"), vec![2]);
    assert_eq!(
        ids(&core, "NOT (s SIMILAR TO 'src/%' OR s SIMILAR TO 'docs/%')"),
        vec![1, 4, 6]
    );
}

/// 左辺 NULL・パターン `NULL` は否定の有無に関わらず行が返らない。
#[test]
fn null_semantics_follow_three_valued_logic() {
    let (core, _g) = setup("sql55s-null");
    seed(&core);
    for form in [
        "s SIMILAR TO '%'",
        "s NOT SIMILAR TO 'zzz'",
        "NOT (s SIMILAR TO 'zzz')",
    ] {
        assert!(!ids(&core, form).contains(&5), "{form}");
    }
    assert_eq!(ids(&core, "s SIMILAR TO NULL"), Vec::<u64>::new());
    assert_eq!(ids(&core, "s NOT SIMILAR TO NULL"), Vec::<u64>::new());
    assert_eq!(ids(&core, "NOT (s SIMILAR TO NULL)"), Vec::<u64>::new());
}

/// 式左辺・括弧付き左辺・AND／OR との組み合わせ。
#[test]
fn expression_lhs_and_combinations() {
    let (core, _g) = setup("sql55s-expr");
    seed(&core);
    assert_eq!(ids(&core, "lower(s) SIMILAR TO 'src/%'"), vec![1, 2]);
    assert_eq!(
        ids(&core, "(lower(s)) SIMILAR TO 'src/%' AND id = 1"),
        vec![1]
    );
    assert_eq!(ids(&core, "(lower(s)) NOT SIMILAR TO '%/%'"), vec![4, 6]);
    assert_eq!(
        ids(&core, "(s) SIMILAR TO 'docs/%' OR k = 'b'"),
        vec![2, 3, 4, 6]
    );
    assert_eq!(ids(&core, "s SIMILAR TO 'src/%' AND k = 'b'"), vec![2]);
    // 小文字でもキーワードとして読む。
    assert_eq!(ids(&core, "s similar to 'src/%'"), vec![2]);
}

/// `similar`／`to`／`escape` という列名が壊れない（文脈照合の識別子）。
#[test]
fn context_words_remain_usable_as_identifiers() {
    let path = unique_db_path("sql55s-idents");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let _g = CleanupGuard(path);
    ddl(
        &core,
        "CREATE TABLE w (id BIGINT PRIMARY KEY, similar TEXT, \"to\" TEXT, \"escape\" TEXT)",
    )
    .expect("create");
    exec(
        &core,
        "tenant-a",
        "INSERT INTO w (id, similar, \"to\", \"escape\") VALUES (1, 'ab', 'x', 'y') \
         USING OPERATION_ID 'w-1'",
    )
    .expect("insert");
    let q = "SELECT id FROM w WHERE similar SIMILAR TO 'a%' AND \"to\" = 'x' LIMIT 10";
    match exec(&core, "tenant-a", q).expect("select") {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 1),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// `ESCAPE`: 既定 `\`・指定文字・空文字列（エスケープ無し）・多バイト・不正な値。
#[test]
fn escape_clause_forms() {
    let (core, _g) = setup("sql55s-escape");
    insert(&core, 1, Some("50%"), "a");
    insert(&core, 2, Some("500"), "a");
    insert(&core, 3, Some("a_b"), "a");
    insert(&core, 4, Some("axb"), "a");
    insert(&core, 5, Some("a#b"), "a");
    insert(&core, 6, Some("éx"), "a");
    insert(&core, 7, Some("%x"), "a");
    // 既定のエスケープ文字は `\`。
    assert_eq!(ids(&core, "s SIMILAR TO '50\\%'"), vec![1]);
    assert_eq!(ids(&core, "s SIMILAR TO 'a\\_b'"), vec![3]);
    // 指定文字。
    assert_eq!(ids(&core, "s SIMILAR TO '50#%' ESCAPE '#'"), vec![1]);
    assert_eq!(ids(&core, "s SIMILAR TO 'a#_b' ESCAPE '#'"), vec![3]);
    assert_eq!(
        ids(&core, "s NOT SIMILAR TO 'a#_b' ESCAPE '#'"),
        vec![1, 2, 4, 5, 6, 7]
    );
    // `ESCAPE ''` はエスケープ無し（`\` も通常文字）。
    assert_eq!(ids(&core, "s SIMILAR TO 'a_b' ESCAPE ''"), vec![3, 4, 5]);
    assert_eq!(
        ids(&core, "s SIMILAR TO '50\\%' ESCAPE ''"),
        Vec::<u64>::new()
    );
    // 多バイト 1 文字。
    assert_eq!(ids(&core, "s SIMILAR TO 'é_' ESCAPE '|'"), vec![6]);
    assert_eq!(ids(&core, "s SIMILAR TO 'é%x' ESCAPE 'é'"), vec![7]);
    // 末尾の孤立したエスケープ文字は PG と同じく無視される。
    assert_eq!(ids(&core, "s SIMILAR TO '500#' ESCAPE '#'"), vec![2]);
    // 2 文字以上は `22025`。
    assert_eq!(err_code(&core, "s SIMILAR TO 'a' ESCAPE 'ab'"), "22025");
    assert_eq!(err_code(&core, "s SIMILAR TO 'a' ESCAPE '##'"), "22025");
}

/// `#"` による部分抽出用の区切り（照合結果は全体一致のまま）。3 個以上は `2201B`。
#[test]
fn escape_double_quote_separators() {
    let (core, _g) = setup("sql55s-sep");
    seed(&core);
    assert_eq!(
        ids(&core, "s SIMILAR TO 'src/#\"main#\"' ESCAPE '#'"),
        vec![2]
    );
    assert_eq!(ids(&core, "s SIMILAR TO '#\"s#\"rc/%' ESCAPE '#'"), vec![2]);
    assert_eq!(
        err_code(&core, "s SIMILAR TO '#\"a#\"b#\"c#\"' ESCAPE '#'"),
        "2201B"
    );
}

/// 不正パターンは `2201B`、上限超過は `54000`（行が 0 件でも束縛時に拒否）。
#[test]
fn pattern_errors_map_to_sqlstate() {
    let (core, _g) = setup("sql55s-errors");
    seed(&core);
    assert_eq!(err_code(&core, "s SIMILAR TO '('"), "2201B");
    assert_eq!(err_code(&core, "s SIMILAR TO 'a)'"), "2201B");
    assert_eq!(err_code(&core, "s NOT SIMILAR TO '['"), "2201B");
    assert_eq!(err_code(&core, "s SIMILAR TO 'a{2,1}'"), "2201B");
    assert_eq!(err_code(&core, "s SIMILAR TO '*a'"), "2201B");
    assert_eq!(err_code(&core, "s SIMILAR TO '(a)\\1'"), "2201B");
    let long = "a".repeat(4097);
    assert_eq!(err_code(&core, &format!("s SIMILAR TO '{long}'")), "54000");
    assert_eq!(err_code(&core, "id < 0 AND s SIMILAR TO '('"), "2201B");
}

/// `with_regex_limits` で下げた上限が運ばれる（変換後の ARE 長に対する検査）。
#[test]
fn configured_limits_apply() {
    let (core, _g) = build("sql55s-limits", Some(RegexLimits::new(16, 8, 1024 * 1024)));
    insert(&core, 1, Some("abcdefgh"), "a");
    insert(&core, 2, Some("abcdefghi"), "a");
    assert_eq!(err_code(&core, "s SIMILAR TO 'abcdefghijklmnop'"), "54000");
    // 入力長 9 の行は評価時に 54000。
    assert_eq!(err_code(&core, "s SIMILAR TO 'abc%'"), "54000");
    assert_eq!(ids(&core, "id = 1 AND s SIMILAR TO 'abc%'"), vec![1]);
}

/// 型・照合: 非 TEXT 左辺は `42883`、ICU 照合は `0A000`、`COLLATE "C"` は受理。
#[test]
fn type_and_collation_rules() {
    let (core, _g) = setup("sql55s-types");
    seed(&core);
    assert_eq!(err_code(&core, "n SIMILAR TO '1'"), "42883");
    assert_eq!(err_code(&core, "iu SIMILAR TO 'a'"), "0A000");
    assert_eq!(
        err_code(&core, "s COLLATE \"und-x-icu\" SIMILAR TO 'a'"),
        "0A000"
    );
    assert_eq!(ids(&core, "s COLLATE \"C\" SIMILAR TO 'src/%'"), vec![2]);
}

/// 述語形 `UPDATE`／`DELETE`: 対象の正しさと、同一 operation_id の再送冪等・内容違いの検出。
#[test]
fn predicate_update_and_delete() {
    let (core, _g) = setup("sql55s-dml");
    seed(&core);
    let upd = "UPDATE t SET k = 'hit' WHERE s SIMILAR TO '(S|s)rc/%' USING OPERATION_ID 'u-1'";
    exec(&core, "tenant-a", upd).expect("update");
    assert_eq!(ids(&core, "k = 'hit'"), vec![1, 2]);
    assert_eq!(exec(&core, "tenant-a", upd).expect_err("replay"), "23505");
    for changed in [
        "UPDATE t SET k = 'hit' WHERE s SIMILAR TO 'docs/%' USING OPERATION_ID 'u-1'",
        "UPDATE t SET k = 'hit' WHERE s NOT SIMILAR TO '(S|s)rc/%' USING OPERATION_ID 'u-1'",
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
        "DELETE FROM t WHERE s NOT SIMILAR TO '(S|s)rc/%' USING OPERATION_ID 'd-1'",
    )
    .expect("delete");
    // NULL 行（id 5）は `NOT SIMILAR TO` でも対象外。
    assert_eq!(ids(&core, "id > 0"), vec![1, 2, 5]);
}

/// RLS: 他テナントの行は照合対象にならない。
#[test]
fn other_tenants_rows_are_never_visible() {
    let (core, _g) = setup("sql55s-rls");
    seed(&core);
    exec(
        &core,
        "tenant-b",
        "INSERT INTO t (id, s, k, n, iu) VALUES (100, 'src/other', 'z', 100, 'x') \
         USING OPERATION_ID 'b-ins'",
    )
    .expect("tenant-b insert");
    assert_eq!(ids(&core, "s SIMILAR TO 'src/%'"), vec![2]);
    match exec(
        &core,
        "tenant-b",
        "SELECT id FROM t WHERE s SIMILAR TO '%' LIMIT 100",
    )
    .expect("tenant-b select")
    {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 1),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 未対応の位置・形は fail-closed（SELECT リスト・CHECK・索引の述語・パターンが列／式・
/// `ESCAPE NULL`・`::` 付き）。
#[test]
fn unsupported_forms_fail_closed() {
    let (core, _g) = setup("sql55s-closed");
    seed(&core);
    assert_eq!(
        exec(&core, "tenant-a", "SELECT s SIMILAR TO 'a' FROM t LIMIT 10").expect_err("projection"),
        "42601"
    );
    assert_eq!(
        ddl(&core, "CREATE INDEX ix_p ON t (k) WHERE s SIMILAR TO 'a'").expect_err("partial"),
        "0A000"
    );
    assert_eq!(
        ddl(
            &core,
            "ALTER TABLE t ADD CONSTRAINT c1 CHECK (s SIMILAR TO 'a')"
        )
        .expect_err("check"),
        "0A000"
    );
    assert_eq!(err_code(&core, "s SIMILAR TO k"), "42601");
    assert_eq!(err_code(&core, "s SIMILAR TO 'a'::text"), "42601");
    assert_eq!(err_code(&core, "s SIMILAR TO 1"), "42601");
    assert_eq!(err_code(&core, "s SIMILAR TO 'a' || 'b'"), "42601");
    assert_eq!(err_code(&core, "s SIMILAR TO 'a' ESCAPE NULL"), "42601");
    assert_eq!(err_code(&core, "s SIMILAR TO 'a' ESCAPE k"), "42601");
    assert_eq!(
        err_code(&core, "s SIMILAR TO 'a' ESCAPE '#'::text"),
        "42601"
    );
    assert_eq!(err_code(&core, "s SIMILAR 'a'"), "42601");
    assert_eq!(err_code(&core, "s SIMILAR TO"), "42601");
}

/// JOIN・HAVING・CASE の条件・ビュー本体・式索引は対象外で `42601`。
/// （サブクエリ内側の素の Scan は Issue #1853 で `LIMIT` 省略を受理したため通常の WHERE と同じ。）
#[test]
fn other_contexts_fail_closed() {
    let (core, _g) = setup("sql55s-other");
    seed(&core);
    for q in [
        "SELECT a.id FROM t a JOIN t b ON a.id = b.id WHERE a.s SIMILAR TO 'src%' LIMIT 10",
        "SELECT k FROM t GROUP BY k HAVING k SIMILAR TO 'a' LIMIT 10",
        "SELECT CASE WHEN s SIMILAR TO 'a' THEN 1 ELSE 0 END FROM t LIMIT 10",
    ] {
        assert_eq!(exec(&core, "tenant-a", q).expect_err(q), "42601", "{q}");
    }
    for q in [
        "CREATE VIEW v AS SELECT id FROM t WHERE s SIMILAR TO 'a'",
        "CREATE INDEX ix ON t ((s SIMILAR TO 'a'))",
    ] {
        assert_eq!(ddl(&core, q).expect_err(q), "42601", "{q}");
    }
}

/// 拡張クエリのバインドパラメータは `SIMILAR TO` の位置では受理しない（`sql::params` に
/// 対応する形が無く、`$n` を使うと Parse で拒否される。fail-closed）。
#[test]
fn bind_parameters_are_rejected_at_parse() {
    let (core, _g) = setup("sql55s-params");
    for sql in [
        "SELECT id FROM t WHERE s SIMILAR TO $1 LIMIT 5",
        "SELECT id FROM t WHERE s SIMILAR TO 'a' ESCAPE $1 LIMIT 5",
        "SELECT id FROM t WHERE $1 SIMILAR TO 'a' LIMIT 5",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err(sql);
        assert!(
            matches!(err.wire_code(), "42601" | "42P18"),
            "{sql}: unexpected code {}",
            err.wire_code()
        );
    }
}

/// エラー文言にパターンの断片が含まれない（固定文言）。
#[test]
fn error_messages_do_not_echo_the_pattern() {
    let (core, _g) = setup("sql55s-msg");
    seed(&core);
    let sql = "SELECT id FROM t WHERE s SIMILAR TO 'secret-(' LIMIT 10";
    let err = core
        .execute_sql_in_session(&ctx("tenant-a"), &mut SessionState::default(), sql)
        .expect_err("invalid pattern");
    assert!(!err.to_string().contains("secret"), "{err}");
}
