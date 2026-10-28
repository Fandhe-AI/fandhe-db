//! 部分索引の格納・DML 維持の結合テスト（INDEX-11、Issue #1654）。
//! ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-11、`docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_partial_index.rs`（宣言側の分類）・`sql_expression_index.rs` と同じ流儀（実 `Storage` ＋
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約
//! （詳細は `docs/design/partial-index.md`）:
//!
//! - 決定的な述語は書き込みトランザクション内で束縛して正規化テキストを永続化し、再オープン後も残る
//! - 束縛段の分類（未知列 `42703`・予約列 `42601`・セッション依存の述語 `42P17`・日時の特殊値 `22007`）
//! - DML は述語を満たす行だけを索引化する。述語外の行では式要素を評価せず（退場・入場）、述語の評価
//!   エラーは `22012` で文を拒否して副作用を残さない
//! - `CREATE INDEX` は保存前に既存行（他テナント・不可視行を含む）を検証し、応答は何も漏らさない
//! - 部分索引は述語を含意しないクエリ・広域取得の結果と EXPLAIN に影響しない（含意するクエリの選択は
//!   `partial_index_planner.rs`、Issue #1656）
//! - 列 DDL（`DROP`・`RENAME`・`ALTER TYPE`）と表 DDL（改名・`DROP TABLE`）への追随
//!
//! クラッシュ整合は `examples/crash_tool_unique_index.rs`（`make crash-test-unique-index`）が担う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
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

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx() -> PolicyContext {
    ctx_of("alice")
}

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn open_fixture(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (path, guard)
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), session, sql)
}

fn code_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    match run(core, session, sql) {
        Ok(outcome) => panic!("{sql} must fail, got {outcome:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn list(path: &std::path::Path) -> Vec<engine::catalog::IndexDef> {
    Storage::open(path)
        .expect("reopen storage")
        .list_indexes()
        .expect("list indexes")
}

fn count_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> usize {
    match run(core, session, sql).expect("select") {
        SqlOutcome::Query(q) => q.rows.len(),
        other => panic!("unexpected outcome {other:?}"),
    }
}

/// `xt (n INTEGER, code TEXT)` を作ったフィクスチャ（索引は張らない）。
fn xt_fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore, SessionState) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    let mut session = allowed_session();
    run(
        &core,
        &mut session,
        "CREATE TABLE xt (id BIGINT PRIMARY KEY, n INTEGER, code TEXT)",
    )
    .expect("create table");
    (path, guard, core, session)
}

fn insert(
    core: &EngineCore,
    session: &mut SessionState,
    id: u64,
    n: i64,
    op: &str,
) -> Result<(), String> {
    run(
        core,
        session,
        &format!("INSERT INTO xt (id, n, code) VALUES ({id}, {n}, 'a') USING OPERATION_ID '{op}'"),
    )
    .map(|_| ())
    .map_err(|e| e.wire_code().to_string())
}

/// 決定的な述語の部分索引は作成でき、正規化テキストが永続化され、再オープン後も残り、`IF NOT EXISTS`・
/// 名前衝突・`DROP INDEX` が通常の索引と同じに振る舞う。
#[test]
fn deterministic_partial_index_is_created_persisted_and_dropped() {
    let (path, _guard) = open_fixture("partial-index-create");
    let core = core_at(&path);
    let mut s = allowed_session();
    for sql in [
        "CREATE INDEX p1 ON docs (lang) WHERE lang = 'ja'",
        "CREATE INDEX p2 ON docs (lang) WHERE lower(lang) = 'ja'",
        "CREATE INDEX p3 ON docs (lang) WHERE body IS NOT NULL AND lang IN ('a', 'b')",
        "CREATE INDEX p4 ON docs (lang) WHERE lang = 'a' OR NOT (body = 'b')",
        "CREATE INDEX p5 ON docs (lang) WHERE id BETWEEN 1 AND 5",
        "CREATE INDEX p6 ON docs ((lower(lang))) WHERE lang = 'ja'",
    ] {
        run(&core, &mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    run(
        &core,
        &mut s,
        "CREATE INDEX IF NOT EXISTS p1 ON docs (lang) WHERE lang = 'ja'",
    )
    .expect("if not exists");
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "CREATE INDEX p1 ON docs (lang) WHERE lang = 'ja'"
        ),
        "42P07"
    );
    drop(core);
    let defs = list(&path);
    assert_eq!(defs.len(), 6);
    let p1 = defs.iter().find(|d| d.name == "p1").expect("p1");
    let pred = p1.predicate.as_ref().expect("predicate is persisted");
    assert_eq!(pred.sql, "lang = 'ja'");
    assert_eq!(pred.referenced_columns, vec!["lang".to_string()]);
    let p5 = defs.iter().find(|d| d.name == "p5").expect("p5");
    assert!(
        p5.predicate
            .as_ref()
            .expect("predicate")
            .referenced_columns
            .is_empty(),
        "pseudo column id is not a schema column"
    );
    let p6 = defs.iter().find(|d| d.name == "p6").expect("p6");
    assert!(p6.has_expressions() && p6.has_predicate());
    let core = core_at(&path);
    run(&core, &mut s, "DROP INDEX p1").expect("drop");
    drop(core);
    assert_eq!(list(&path).len(), 5);
}

/// 束縛段の分類: 未知列 `42703`・予約列 `42601`。拒否後に宣言は残らない。
#[test]
fn predicate_binding_errors_are_transparent_and_leave_no_declaration() {
    let (path, _guard) = open_fixture("partial-index-bind");
    let core = core_at(&path);
    let mut s = allowed_session();
    for (sql, code) in [
        ("CREATE INDEX i ON docs (lang) WHERE nope = 'x'", "42703"),
        (
            "CREATE INDEX i ON docs (lang) WHERE lower(nope) = 'x'",
            "42703",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE tenant_id = 'a'",
            "42601",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE visibility = 'public'",
            "42601",
        ),
    ] {
        assert_eq!(code_of(&core, &mut s, sql), code, "{sql}");
    }
    drop(core);
    assert!(list(&path).is_empty());
}

/// 型に依存する決定性の再判定（`42P17`）: `TIMESTAMPTZ` 列とリテラルの比較・日時の特殊値。
#[test]
fn session_dependent_predicates_are_rejected_at_binding() {
    let path = unique_db_path("partial-index-tz");
    let _guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    let mut s = allowed_session();
    run(
        &core,
        &mut s,
        "CREATE TABLE ev (id BIGINT PRIMARY KEY, at_tz TIMESTAMPTZ, at TIMESTAMP, label TEXT)",
    )
    .expect("create table");
    for sql in [
        "CREATE INDEX i ON ev (label) WHERE at_tz > '2020-01-01 00:00:00'",
        "CREATE INDEX i ON ev (label) WHERE at_tz = '2020-01-01 00:00:00+00'",
    ] {
        assert_eq!(code_of(&core, &mut s, sql), "42P17", "{sql}");
    }
    // 日時の特殊値リテラルは束縛が受理せず（`22007`）、評価時刻に依存する述語は作れない。
    for sql in [
        "CREATE INDEX i ON ev (label) WHERE at > 'now'",
        "CREATE INDEX i ON ev (label) WHERE at > 'today'",
    ] {
        assert_eq!(code_of(&core, &mut s, sql), "22007", "{sql}");
    }
    run(
        &core,
        &mut s,
        "CREATE INDEX ok1 ON ev (label) WHERE at > '2020-01-01 00:00:00'",
    )
    .expect("naive timestamp literal is deterministic");
}

/// DML の入場・退場: 述語外の行は受理され、述語外の行では失敗する式要素を評価しない。UPDATE で述語を
/// 外れる（退場）行・述語内へ入る（入場）行の双方が、式要素の評価可否に従って扱われる。
#[test]
fn rows_enter_and_leave_the_partial_index_on_dml() {
    let (_path, _guard, core, mut s) = xt_fixture("partial-index-dml-enter");
    run(
        &core,
        &mut s,
        "CREATE INDEX xt_div ON xt ((100 / n)) WHERE n <> 0",
    )
    .expect("create partial index");
    insert(&core, &mut s, 1, 5, "op-1").expect("in-predicate row");
    insert(&core, &mut s, 2, 0, "op-2").expect("out-of-predicate row must be accepted");
    assert_eq!(count_of(&core, &mut s, "SELECT id FROM xt LIMIT 100"), 2);
    run(
        &core,
        &mut s,
        "UPDATE xt SET n = 0 WHERE id = 1 USING OPERATION_ID 'op-u1'",
    )
    .expect("exit the partial index");
    run(
        &core,
        &mut s,
        "UPDATE xt SET n = 4 WHERE id = 2 USING OPERATION_ID 'op-u2'",
    )
    .expect("enter the partial index");
    insert(&core, &mut s, 3, 0, "op-3").expect("another out-of-predicate row");
    run(
        &core,
        &mut s,
        "UPDATE xt SET code = 'z' WHERE id = 3 USING OPERATION_ID 'op-u3'",
    )
    .expect("non-referenced update");
}

/// 述語自体の評価エラー（`100 / (n + 5) > 1` で `n = -5`）は INSERT・UPDATE・UPSERT・明示トランザクション
/// のいずれでも `22012` で文を拒否し、行を残さない。
#[test]
fn predicate_evaluation_errors_reject_the_statement() {
    let (_path, _guard, core, mut s) = xt_fixture("partial-index-dml-pred-err");
    run(
        &core,
        &mut s,
        "CREATE INDEX xt_p ON xt (code) WHERE 100 / (n + 5) > 1",
    )
    .expect("create partial index");
    insert(&core, &mut s, 1, 1, "op-1").expect("valid row");
    assert_eq!(
        insert(&core, &mut s, 2, -5, "op-2"),
        Err("22012".to_string())
    );
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "UPDATE xt SET n = -5 WHERE id = 1 USING OPERATION_ID 'op-u'"
        ),
        "22012"
    );
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "INSERT INTO xt (id, n, code) VALUES (1, -5, 'a') ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n USING OPERATION_ID 'op-up'"
        ),
        "22012"
    );
    assert_eq!(count_of(&core, &mut s, "SELECT id FROM xt LIMIT 100"), 1);
    assert_eq!(
        count_of(&core, &mut s, "SELECT id FROM xt WHERE n = 1 LIMIT 100"),
        1,
        "the original row keeps its value"
    );
    let caller = ctx();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            "INSERT INTO xt (id, n, code) VALUES (7, -5, 'a') USING OPERATION_ID 'op-tx'",
        )
        .expect_err("failing predicate");
    assert_eq!(err.wire_code(), "22012");
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count_of(&core, &mut s, "SELECT id FROM xt LIMIT 100"), 1);
}

/// 述語 TRUE の行で式要素が評価エラーになる UPDATE（入場）は `22012` で拒否され、行は無変化。
#[test]
fn entering_row_with_failing_element_is_rejected() {
    let (_path, _guard, core, mut s) = xt_fixture("partial-index-dml-enter-fail");
    run(
        &core,
        &mut s,
        "CREATE INDEX xt_e ON xt ((100 / n)) WHERE code = 'on'",
    )
    .expect("create");
    run(
        &core,
        &mut s,
        "INSERT INTO xt (id, n, code) VALUES (1, 0, 'off') USING OPERATION_ID 'op-1'",
    )
    .expect("outside the predicate");
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "UPDATE xt SET code = 'on' WHERE id = 1 USING OPERATION_ID 'op-2'"
        ),
        "22012"
    );
    assert_eq!(
        count_of(
            &core,
            &mut s,
            "SELECT id FROM xt WHERE code = 'off' LIMIT 100"
        ),
        1,
        "row is unchanged"
    );
    run(
        &core,
        &mut s,
        "UPDATE xt SET n = 5, code = 'on' WHERE id = 1 USING OPERATION_ID 'op-3'",
    )
    .expect("a valid element value may enter");
}

/// `CREATE INDEX` は保存前に既存行（他テナント・不可視行を含む）で述語を評価する。評価エラーは固定文言で
/// 拒否し、宣言は残らず、応答はテナント名を含まない。述語外の行の式要素は評価しない。
#[test]
fn create_index_validates_existing_rows_across_tenants() {
    let (_path, _guard, core, mut s) = xt_fixture("partial-index-existing");
    core.execute_sql_in_session(
        &ctx_of("bob-secret"),
        &mut allowed_session(),
        "INSERT INTO xt (id, n, code) VALUES (1, -5, 'x') USING OPERATION_ID 'op-bob'",
    )
    .expect("bob row");
    let err = run(
        &core,
        &mut s,
        "CREATE INDEX xt_p ON xt (code) WHERE 100 / (n + 5) > 1",
    )
    .expect_err("existing failing row must reject the index");
    assert_eq!(err.wire_code(), "22012");
    assert!(
        !err.to_string().contains("bob-secret"),
        "must not echo the tenant: {err}"
    );
    assert_eq!(code_of(&core, &mut s, "DROP INDEX xt_p"), "42704");
    run(
        &core,
        &mut s,
        "CREATE INDEX xt_ok ON xt ((100 / (n + 5))) WHERE n <> -5",
    )
    .expect("failing element row is outside the predicate");
    let err = run(
        &core,
        &mut s,
        "CREATE INDEX xt_bad ON xt ((100 / (n + 5))) WHERE n < 0",
    )
    .expect_err("failing element inside the predicate");
    assert_eq!(err.wire_code(), "22012");
}

/// DML ゲートは自テナントの書き込み行だけを評価し、他テナントの行・値・id を応答へ出さない。
#[test]
fn dml_gate_does_not_touch_other_tenants() {
    let (_path, _guard, core, mut s) = xt_fixture("partial-index-tenant");
    run(
        &core,
        &mut s,
        "CREATE INDEX xt_p ON xt (code) WHERE 100 / (n + 5) > 1",
    )
    .expect("create");
    core.execute_sql_in_session(
        &ctx_of("bob-secret"),
        &mut allowed_session(),
        "INSERT INTO xt (id, n, code) VALUES (1, 3, 'bob') USING OPERATION_ID 'op-bob'",
    )
    .expect("bob row");
    let err = core
        .execute_sql_in_session(
            &ctx_of("bob-secret"),
            &mut allowed_session(),
            "INSERT INTO xt (id, n, code) VALUES (2, -5, 'bob') USING OPERATION_ID 'op-bob-bad'",
        )
        .expect_err("bob's failing row");
    assert_eq!(err.wire_code(), "22012");
    let text = err.to_string();
    assert!(
        !text.contains("bob-secret") && !text.contains("-5"),
        "no tenant or value in the response: {text}"
    );
    insert(&core, &mut s, 1, 4, "op-alice").expect("alice writes are unaffected");
}

/// 述語を含意しないクエリ（`n >= 0` は `n > 1` を含意しない）では、部分索引の有無で結果集合が変わらず、
/// EXPLAIN に名前が出ない（含意するクエリの選択と表示は `partial_index_planner.rs`、Issue #1656）。
#[test]
fn partial_index_is_invisible_to_non_implying_queries() {
    let (_path, _guard, core, mut s) = xt_fixture("partial-index-planner");
    for (id, n) in [(1u64, 1i64), (2, 2), (3, 3), (4, 0)] {
        insert(&core, &mut s, id, n, &format!("op-{id}")).expect("row");
    }
    let query = "SELECT id FROM xt WHERE n >= 0 LIMIT 100";
    let before = count_of(&core, &mut s, query);
    run(&core, &mut s, "CREATE INDEX xt_p ON xt (n) WHERE n > 1").expect("create");
    assert_eq!(count_of(&core, &mut s, query), before);
    assert_eq!(
        count_of(&core, &mut s, "SELECT id FROM xt WHERE n > 0 LIMIT 100"),
        3
    );
    let explain = format!(
        "{:?}",
        run(&core, &mut s, &format!("EXPLAIN {query}")).expect("explain")
    );
    assert!(
        !explain.contains("xt_p"),
        "partial index must not be named: {explain}"
    );
}

/// 列 DDL: 述語が参照する列の `ALTER COLUMN TYPE` は `0A000` で無変化、`RENAME COLUMN` は述語テキストの
/// 追随（入れ替えを含む）、`DROP COLUMN` は索引ごと連鎖削除。改名後も DML が通る。
#[test]
fn column_ddl_follows_the_partial_index() {
    let (path, _guard, core, mut s) = xt_fixture("partial-index-column-ddl");
    run(
        &core,
        &mut s,
        "CREATE INDEX xt_p ON xt (code) WHERE n > 0 AND code <> 'z'",
    )
    .expect("create");
    drop(core);
    let before = list(&path);
    let core = core_at(&path);
    assert_eq!(
        code_of(&core, &mut s, "ALTER TABLE xt ALTER COLUMN n TYPE BIGINT"),
        "0A000"
    );
    drop(core);
    assert_eq!(
        list(&path),
        before,
        "ALTER TYPE rejection leaves the index unchanged"
    );
    let core = core_at(&path);
    run(&core, &mut s, "ALTER TABLE xt RENAME COLUMN n TO m").expect("rename");
    drop(core);
    let renamed = list(&path);
    let pred = renamed[0].predicate.as_ref().expect("predicate");
    assert!(
        pred.sql.contains("m >") && !pred.sql.contains("n >"),
        "{}",
        pred.sql
    );
    assert_eq!(
        pred.referenced_columns,
        vec!["m".to_string(), "code".to_string()]
    );
    let core = core_at(&path);
    run(
        &core,
        &mut s,
        "INSERT INTO xt (id, m, code) VALUES (1, 5, 'a') USING OPERATION_ID 'op-after-rename'",
    )
    .expect("DML after rename re-binds the stored predicate");
    // 列名の入れ替え（m→tmp, code→m, tmp→code）でも連鎖置換は起きない。
    run(&core, &mut s, "ALTER TABLE xt RENAME COLUMN m TO tmp").expect("swap 1");
    run(&core, &mut s, "ALTER TABLE xt RENAME COLUMN code TO m").expect("swap 2");
    run(&core, &mut s, "ALTER TABLE xt RENAME COLUMN tmp TO code").expect("swap 3");
    drop(core);
    let swapped = list(&path);
    let pred = swapped[0].predicate.as_ref().expect("predicate");
    assert!(
        pred.sql.contains("code >") && pred.sql.contains("NOT m = 'z'"),
        "{}",
        pred.sql
    );
    let core = core_at(&path);
    run(&core, &mut s, "ALTER TABLE xt DROP COLUMN code").expect("drop column");
    drop(core);
    assert!(list(&path).is_empty());
}

/// 表改名・`DROP TABLE` は部分索引に追随する。
#[test]
fn table_rename_and_drop_follow_the_partial_index() {
    let (path, _guard, core, mut s) = xt_fixture("partial-index-table-ddl");
    run(&core, &mut s, "CREATE INDEX xt_p ON xt (code) WHERE n > 0").expect("create");
    run(&core, &mut s, "ALTER TABLE xt RENAME TO xt2").expect("rename table");
    run(
        &core,
        &mut s,
        "INSERT INTO xt2 (id, n, code) VALUES (1, 5, 'a') USING OPERATION_ID 'op-1'",
    )
    .expect("DML after table rename");
    drop(core);
    let defs = list(&path);
    assert_eq!(defs[0].table, "xt2");
    assert!(defs[0].has_predicate());
    let core = core_at(&path);
    run(&core, &mut s, "DROP TABLE xt2").expect("drop table");
    drop(core);
    assert!(list(&path).is_empty());
}

/// `Storage::create_index`（Rust API）は述語付きの宣言を拒否する（束縛を通る SQL 表層だけが作れる）。
#[test]
fn rust_api_rejects_predicate_declarations() {
    let (path, _guard) = open_fixture("partial-index-api");
    let storage = Storage::open(&path).expect("open");
    let def = engine::catalog::IndexDef::new(
        "i".to_string(),
        "docs".to_string(),
        engine::catalog::IndexKind::Scalar,
        vec!["lang".to_string()],
    )
    .with_predicate(Some(engine::catalog::IndexPredicateDef {
        sql: "lang = 'ja'".to_string(),
        referenced_columns: vec!["lang".to_string()],
    }));
    storage.create_index(&def).expect_err("must be rejected");
    assert!(storage.list_indexes().expect("list").is_empty());
}

/// 再オープン（クラッシュ後の再起動と同じ読み直し）後も、述語は行と一致する形で維持される。
#[test]
fn partial_index_survives_reopen_and_keeps_gating_dml() {
    let (path, _guard, core, mut s) = xt_fixture("partial-index-reopen");
    run(
        &core,
        &mut s,
        "CREATE INDEX xt_p ON xt (code) WHERE 100 / (n + 5) > 1",
    )
    .expect("create");
    insert(&core, &mut s, 1, 1, "op-1").expect("row");
    drop(core);
    let core = core_at(&path);
    let mut s = allowed_session();
    assert_eq!(
        insert(&core, &mut s, 2, -5, "op-2"),
        Err("22012".to_string())
    );
    insert(&core, &mut s, 3, 2, "op-3").expect("valid row after reopen");
    assert_eq!(count_of(&core, &mut s, "SELECT id FROM xt LIMIT 100"), 2);
}

/// 一時表の部分索引も永続表と同じ入退場・評価エラーになる（TABLE-44 (5) ポインタ）。
#[test]
fn temp_table_partial_index_gates_dml_like_a_persistent_table() {
    let path = unique_db_path("partial-index-temp");
    let _guard = CleanupGuard(path.clone());
    // 一時表の DML は台帳を使わない（`USING OPERATION_ID` は受け付けない）。
    let core = core_at(&path).with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let mut s = allowed_session();
    run(
        &core,
        &mut s,
        "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, n INTEGER, code TEXT)",
    )
    .expect("create temp table");
    run(
        &core,
        &mut s,
        "CREATE INDEX tt_p ON tt ((100 / n)) WHERE n <> 0",
    )
    .expect("create partial index on a temp table");
    run(
        &core,
        &mut s,
        "INSERT INTO tt (id, n, code) VALUES (1, 0, 'a'), (2, 5, 'b')",
    )
    .expect("out-of-predicate row is accepted");
    run(
        &core,
        &mut s,
        "CREATE INDEX tt_q ON tt (code) WHERE 100 / (n + 5) > 1",
    )
    .expect("create second partial index");
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "INSERT INTO tt (id, n, code) VALUES (3, -5, 'c')"
        ),
        "22012"
    );
    assert_eq!(
        code_of(&core, &mut s, "UPDATE tt SET n = -5 WHERE id = 2"),
        "22012"
    );
    assert_eq!(count_of(&core, &mut s, "SELECT id FROM tt LIMIT 100"), 2);
}

/// matview への部分索引は `0A000`（述語を黙って落とさない）。宣言は残らない。
#[test]
fn partial_index_on_materialized_view_is_rejected() {
    let (path, _guard) = open_fixture("partial-index-matview");
    let core = core_at(&path);
    let mut s = allowed_session();
    run(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    )
    .expect("create matview");
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "CREATE INDEX mv_p ON mv (body) WHERE body = 'x'"
        ),
        "0A000"
    );
    drop(core);
    assert!(list(&path).is_empty());
}
