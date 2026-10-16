//! 列定義・式の `COLLATE` 句の構文解析とカタログ保存（TABLE-28、Issue #1609）の SQL 表層
//! 結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（構文検証 → DDL 権限ゲート → `sql::ddl`）を
//! production 経路として検証し、再起動後（`Storage` の開き直し）に照合が復元されることを
//! カタログ API で確認する。受理するのは `C`／`POSIX`／`und-x-icu`／`ja-x-icu` のみで、それ以外は
//! `42704`（fail-closed）。`C`／`POSIX` の比較・ソートの意味論は変えない（`COLLATE` なしと同じ結果）。
//! ICU 照合の比較・順序は `sql_collate_icu.rs` が検証する。

use engine::catalog::{ColumnType, TableSchema};
use engine::collation::Collation;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("owner", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), session, sql)
}

fn code_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    run(core, session, sql)
        .expect_err(&format!("expected {sql:?} to be rejected"))
        .wire_code()
}

fn texts(core: &EngineCore, sql: &str) -> Vec<String> {
    core.execute_sql(&ctx(), sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            Cell::Text(s) => s.clone(),
            other => panic!("unexpected cell: {other:?}"),
        })
        .collect()
}

fn schema_of(path: &std::path::Path, table: &str) -> TableSchema {
    let storage = Storage::open(path).expect("reopen storage");
    storage.get_table_schema(table).expect("schema")
}

fn collations(schema: &TableSchema) -> Vec<(String, Option<Collation>)> {
    schema
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.collation))
        .collect()
}

const CREATE: &str = "CREATE TABLE t (embedding VECTOR(2), a TEXT COLLATE \"C\", \
     b VARCHAR(10) COLLATE \"POSIX\", c CHAR(3) COLLATE \"C\", d TEXT)";

fn seed(core: &EngineCore) {
    for (i, v) in ["banana", "Apple", "cherry", "apple"].iter().enumerate() {
        let mut s = SessionState::default();
        run(
            core,
            &mut s,
            &format!(
                "INSERT INTO t (id, embedding, a) VALUES ({}, '[0.1,0.2]', '{v}') \
                 USING OPERATION_ID 'op-{i}'",
                i + 1
            ),
        )
        .expect("insert");
    }
}

/// TABLE-28: 再起動後に列の照合が復元される（受け入れ条件）。CREATE／ADD COLUMN／
/// ALTER COLUMN TYPE のいずれで付けた照合も、明示なし（`None`）と区別して往復する。
#[test]
fn collations_survive_restart() {
    let path = unique_db_path("sql-collate-ddl-restart");
    let _guard = CleanupGuard(path.clone());
    {
        let core = open_core(&path);
        let mut session = ddl_session();
        run(&core, &mut session, CREATE).expect("create");
        run(
            &core,
            &mut session,
            "ALTER TABLE t ADD COLUMN e TEXT COLLATE \"POSIX\"",
        )
        .expect("add column");
        run(
            &core,
            &mut session,
            "ALTER TABLE t ALTER COLUMN d TYPE VARCHAR(5) COLLATE \"C\"",
        )
        .expect("alter type");
        seed(&core);
    }
    let schema = schema_of(&path, "t");
    assert_eq!(
        collations(&schema),
        vec![
            ("embedding".to_string(), None),
            ("a".to_string(), Some(Collation::C)),
            ("b".to_string(), Some(Collation::Posix)),
            ("c".to_string(), Some(Collation::C)),
            ("d".to_string(), Some(Collation::C)),
            ("e".to_string(), Some(Collation::Posix)),
        ]
    );
    assert_eq!(schema.columns[4].ty, ColumnType::Varchar(Some(5)));
    // 再起動後も従来どおり動く（比較・ソートの意味論は不変＝バイト列順）。
    let core = open_core(&path);
    assert_eq!(
        texts(&core, "SELECT a FROM t ORDER BY a LIMIT 100"),
        vec!["Apple", "apple", "banana", "cherry"]
    );
}

/// 照合だけの ALTER（型が同じ）はカタログのみ書き換える。`COLLATE` 省略は照合指定なしへ戻す。
#[test]
fn alter_type_changes_and_resets_collation() {
    let path = unique_db_path("sql-collate-ddl-alter");
    let _guard = CleanupGuard(path.clone());
    {
        let core = open_core(&path);
        let mut session = ddl_session();
        run(&core, &mut session, CREATE).expect("create");
        run(
            &core,
            &mut session,
            "ALTER TABLE t ALTER COLUMN a TYPE TEXT",
        )
        .expect("reset");
        run(
            &core,
            &mut session,
            "ALTER TABLE t ALTER COLUMN d TYPE TEXT COLLATE \"POSIX\"",
        )
        .expect("change");
        run(
            &core,
            &mut session,
            "ALTER TABLE t ALTER COLUMN b TYPE VARCHAR(20)",
        )
        .expect("widen resets collation");
    }
    let schema = schema_of(&path, "t");
    let got = collations(&schema);
    assert_eq!(got[1], ("a".to_string(), None));
    assert_eq!(got[2], ("b".to_string(), None));
    assert_eq!(got[4], ("d".to_string(), Some(Collation::Posix)));
}

/// 拒否の網羅: 未知の照合 `42704`、照合を持てない型 `42804`、判定順は名前が先、
/// 構文違反 `42601`、対象外 `0A000`。拒否は副作用ゼロ（カタログ不変）。
#[test]
fn rejections_are_fail_closed_and_side_effect_free() {
    let path = unique_db_path("sql-collate-ddl-reject");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut session = ddl_session();
    run(&core, &mut session, CREATE).expect("create");
    run(&core, &mut session, "CREATE TYPE mood AS ENUM ('a', 'b')").expect("enum");
    let before = core
        .execute_sql(&ctx(), "SELECT a FROM t LIMIT 100")
        .map(|o| o.rows.len())
        .expect("select");
    let cases: &[(&str, &str)] = &[
        // 42704: 未知の照合名（大小文字は畳まない）。
        ("CREATE TABLE x1 (a TEXT COLLATE \"xx\")", "42704"),
        ("CREATE TABLE x1 (a TEXT COLLATE \"de_DE\")", "42704"),
        ("CREATE TABLE x1 (a TEXT COLLATE c)", "42704"),
        ("CREATE TABLE x1 (a TEXT COLLATE \"posix\")", "42704"),
        ("ALTER TABLE t ADD COLUMN z TEXT COLLATE \"xx\"", "42704"),
        (
            "ALTER TABLE t ALTER COLUMN a TYPE TEXT COLLATE \"xx\"",
            "42704",
        ),
        // 42804: 照合を持てない型。
        ("CREATE TABLE x1 (a INTEGER COLLATE \"C\")", "42804"),
        ("CREATE TABLE x1 (a VECTOR(3) COLLATE \"C\")", "42804"),
        ("CREATE TABLE x1 (a mood COLLATE \"C\")", "42804"),
        ("ALTER TABLE t ADD COLUMN z INTEGER COLLATE \"C\"", "42804"),
        // 判定順: 名前（42704）が型（42804）より先。
        ("CREATE TABLE x1 (a INTEGER COLLATE \"xx\")", "42704"),
        // 42601: 構文違反。
        (
            "CREATE TABLE x1 (a TEXT COLLATE \"C\" COLLATE \"C\")",
            "42601",
        ),
        ("CREATE TABLE x1 (a TEXT COLLATE)", "42601"),
        ("CREATE TABLE x1 (a TEXT COLLATE 'C')", "42601"),
        // ハイフン入りの引用照合名は照合名の位置でだけ受理する（Issue #2190）。他の位置は従来どおり
        // 引用識別子として使えず 42601。
        ("CREATE TABLE \"und-x-icu\" (a TEXT)", "42601"),
        ("CREATE TABLE x1 (\"und-x-icu\" TEXT)", "42601"),
        // 許可リスト外の ICU 名は 42704。
        ("CREATE TABLE x1 (a TEXT COLLATE \"en-x-icu\")", "42704"),
        ("CREATE TABLE x1 (a TEXT COLLATE \"UND-X-ICU\")", "42704"),
        ("CREATE TABLE x1 (a TEXT COLLATE \"xx-x-icu\")", "42704"),
        ("CREATE TABLE x1 (a TEXT COLLATE pg_catalog.\"C\")", "42601"),
        // 0A000: 対象外。
        ("CREATE TABLE x1 (a TEXT[] COLLATE \"C\")", "0A000"),
        (
            "CREATE TABLE x1 (a TEXT, CHECK (a COLLATE \"C\" = 'x'))",
            "0A000",
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(code_of(&core, &mut session, sql), *want, "{sql}");
    }
    // 副作用ゼロ: 失敗した CREATE は表を作らず、既存表は不変。
    assert_eq!(
        code_of(&core, &mut session, "SELECT a FROM x1 LIMIT 100"),
        "42P01"
    );
    assert_eq!(
        core.execute_sql(&ctx(), "SELECT a FROM t LIMIT 100")
            .map(|o| o.rows.len())
            .expect("select"),
        before
    );
    // ALTER TYPE INTEGER COLLATE は列が TEXT のとき 42804。
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "ALTER TABLE t ALTER COLUMN d TYPE INTEGER COLLATE \"C\""
        ),
        "42804"
    );
    drop(core);
    let schema = schema_of(&path, "t");
    assert_eq!(
        collations(&schema)[1],
        ("a".to_string(), Some(Collation::C))
    );
    assert_eq!(schema.columns[4].ty, ColumnType::Text);
}

/// 式の `COLLATE`: `COLLATE` なしと同じ結果（`C`／`POSIX` はバイト列順と同値）。
#[test]
fn expression_collate_matches_plain_results() {
    let path = unique_db_path("sql-collate-ddl-expr");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut session = ddl_session();
    run(&core, &mut session, CREATE).expect("create");
    seed(&core);
    assert_eq!(
        texts(&core, "SELECT a COLLATE \"C\" FROM t ORDER BY a LIMIT 100"),
        vec!["Apple", "apple", "banana", "cherry"]
    );
    assert_eq!(
        texts(
            &core,
            "SELECT a FROM t WHERE a COLLATE \"POSIX\" = 'apple' LIMIT 100"
        ),
        vec!["apple"]
    );
    assert_eq!(
        texts(
            &core,
            "SELECT a FROM t WHERE a COLLATE \"C\" < 'b' ORDER BY a LIMIT 100"
        ),
        vec!["Apple", "apple"]
    );
    assert_eq!(
        texts(
            &core,
            "SELECT a FROM t ORDER BY a COLLATE \"C\" DESC LIMIT 100"
        ),
        vec!["cherry", "banana", "apple", "Apple"]
    );
    // 異なる明示の照合同士の比較は 42P21（Issue #2190。PostgreSQL と同じ）。
    let mut s = SessionState::default();
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "SELECT a FROM t WHERE a COLLATE \"C\" = 'apple' COLLATE \"POSIX\" LIMIT 100"
        ),
        "42P21"
    );
    assert_eq!(
        texts(
            &core,
            "SELECT a FROM t WHERE a COLLATE \"C\" = 'apple' COLLATE \"C\" LIMIT 100"
        ),
        vec!["apple"]
    );
    assert_eq!(
        code_of(&core, &mut s, "SELECT 1 COLLATE \"C\" FROM t LIMIT 100"),
        "42804"
    );
    assert_eq!(
        code_of(&core, &mut s, "SELECT a COLLATE \"zz\" FROM t LIMIT 100"),
        "42704"
    );
}

/// ビュー本文は再解析されるため、`COLLATE` を含む本体も再起動後に同じ結果になる。
#[test]
fn view_with_collate_survives_restart() {
    let path = unique_db_path("sql-collate-ddl-view");
    let _guard = CleanupGuard(path.clone());
    {
        let core = open_core(&path);
        let mut session = ddl_session();
        run(&core, &mut session, CREATE).expect("create");
        seed(&core);
        run(
            &core,
            &mut session,
            "CREATE VIEW v AS SELECT a FROM t ORDER BY a COLLATE \"C\" LIMIT 100",
        )
        .expect("create view");
    }
    let core = open_core(&path);
    assert_eq!(
        texts(&core, "SELECT a FROM v LIMIT 100"),
        vec!["Apple", "apple", "banana", "cherry"]
    );
}

/// 権限の無い主体の拒否は、テーブルの存在の有無で区別できない（存在オラクルにならない）。
#[test]
fn permission_denial_is_identical_regardless_of_existence() {
    let path = unique_db_path("sql-collate-ddl-perm");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    {
        let mut admin = ddl_session();
        run(&core, &mut admin, CREATE).expect("create");
    }
    let mut session = SessionState::default();
    let mut messages = Vec::new();
    for sql in [
        "ALTER TABLE t ALTER COLUMN a TYPE TEXT COLLATE \"POSIX\"",
        "ALTER TABLE nope ALTER COLUMN a TYPE TEXT COLLATE \"POSIX\"",
        "ALTER TABLE t ADD COLUMN z TEXT COLLATE \"C\"",
        "ALTER TABLE nope ADD COLUMN z TEXT COLLATE \"C\"",
    ] {
        let err = run(&core, &mut session, sql).expect_err("must be denied");
        assert_eq!(err.wire_code(), "42501", "{sql}");
        messages.push(err.to_string());
    }
    assert!(messages.windows(2).all(|w| w[0] == w[1]));
}
