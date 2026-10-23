//! 述語形 `UPDATE`／`DELETE` の `WHERE` 内サブクエリ（構文段の受理と実行経路）の結合テスト
//! （Issue #1750・親 #1749・SQL-53 (a)）。
//!
//! 契約: 構文段は SELECT と同じ AST で受理し、受理した文は #1751 の実行経路
//! （`resolve_predicate_*_subqueries`）が束縛前に解決する。`USING OPERATION_ID` 付きも受理し、
//! 解決前の文の正規化ハッシュで内側の評価前に台帳を事前照合する（Issue #2650・RECOVER-11）:
//! 同一文の再送は `23505`、別文の再利用は `22023`。`PARTITIONED` 併用・`$n` 併用は構文／Parse 段で `42601`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql53-dml-where-subquery");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create docs");
    storage
        .create_table(&TableSchema::new(
            "langs",
            vec![ColumnDef::new("lang", ColumnType::Text, false)],
        ))
        .expect("create langs");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, sql: &str) -> Result<String, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
        .map(|o| format!("{o:?}"))
}

fn seed(core: &EngineCore) {
    for (id, lang) in [(1, "ja"), (2, "en")] {
        run(
            core,
            &format!(
                "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.{id},0.1]', '{lang}') \
                 USING OPERATION_ID 'doc-{id}'"
            ),
        )
        .expect("seed doc");
    }
    run(
        core,
        "INSERT INTO langs (id, lang) VALUES (1, 'ja') USING OPERATION_ID 'lang-1'",
    )
    .expect("seed lang");
}

fn doc_rows(core: &EngineCore) -> String {
    run(core, "SELECT id, lang FROM docs ORDER BY id LIMIT 10").expect("select docs")
}

const FORMS: [&str; 6] = [
    "lang IN (SELECT lang FROM langs LIMIT 10)",
    "lang NOT IN (SELECT lang FROM langs LIMIT 10)",
    "EXISTS (SELECT id FROM langs LIMIT 1)",
    "NOT EXISTS (SELECT id FROM langs LIMIT 1)",
    "lang = (SELECT lang FROM langs LIMIT 1)",
    "lang = 'en' OR EXISTS (SELECT id FROM langs LIMIT 1)",
];

fn assert_42601(err: &SqlSurfaceError) {
    assert_eq!(err.wire_code(), "42601", "{err:?}");
}

/// 各形の期待影響件数（シード: docs = ja/en、langs = {ja}）。UPDATE／DELETE 共通。
const FORM_COUNTS: [usize; 6] = [1, 1, 2, 0, 1, 2];

fn affected_tag(out: &str, verb: &str) -> usize {
    // `Debug` 表記から rows_affected を取り出す（`rows_affected: N`）。
    let _ = verb;
    let i = out.find("rows_affected: ").expect("rows_affected") + "rows_affected: ".len();
    out[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("number")
}

/// `USING OPERATION_ID` 付きの各形は実行され、再送は `23505`、別文の再利用は `22023`（SQL-53・RECOVER-11）。
#[test]
fn subquery_where_dml_with_operation_id_executes_and_resend_is_idempotent() {
    for (i, w) in FORMS.iter().enumerate() {
        for verb in ["DELETE", "UPDATE"] {
            let (core, path) = new_core();
            let _guard = CleanupGuard(path);
            seed(&core);
            let sql = if verb == "DELETE" {
                format!("DELETE FROM docs WHERE {w} USING OPERATION_ID 'op-{i}'")
            } else {
                format!("UPDATE docs SET lang = 'xx' WHERE {w} USING OPERATION_ID 'op-{i}'")
            };
            let out = run(&core, &sql).expect("executes");
            assert_eq!(
                affected_tag(&out, verb),
                FORM_COUNTS[i],
                "{verb} {w}: {out}"
            );
            let after = doc_rows(&core);
            // 内側の表を変えても同一文の再送は 23505（内側の結果に左右されない）。
            run(&core, "DELETE FROM langs WHERE lang = 'ja'").expect("change inner");
            let err = run(&core, &sql).expect_err("resend");
            assert_eq!(err.wire_code(), "23505", "{verb} {w}: {err:?}");
            // 文を変えた再利用は 22023。
            let other = sql.replace(" WHERE ", " WHERE id > 0 AND ");
            let err = run(&core, &other).expect_err("reuse");
            assert_eq!(err.wire_code(), "22023", "{verb} {w}: {err:?}");
            assert_eq!(after, doc_rows(&core));
        }
    }
}

/// `RETURNING` の有無で再送判定は変わらない（`RETURNING` 付きの再送も `23505`）。
#[test]
fn subquery_where_dml_returning_resend_is_23505() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    let sql = "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) \
               RETURNING id USING OPERATION_ID 'ret-1'";
    run(&core, sql).expect("executes");
    let err = run(&core, sql).expect_err("resend");
    assert_eq!(err.wire_code(), "23505", "{err:?}");
}

/// 他テナントの langs／docs は結果・件数・行に影響しない（RLS-10）。
#[test]
fn subquery_where_dml_ignores_other_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    let b = PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let mut sess = SessionState::default();
    for sql in [
        "INSERT INTO langs (id, lang) VALUES (7, 'en') USING OPERATION_ID 'b-l'",
        "INSERT INTO docs (id, embedding, lang) VALUES (7, '[0.7,0.1]', 'en') USING OPERATION_ID 'b-d'",
    ] {
        core.execute_sql_in_session(&b, &mut sess, sql).expect("seed b");
    }
    let out = run(
        &core,
        "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) USING OPERATION_ID 'x-1'",
    )
    .expect("delete");
    assert_eq!(affected_tag(&out, "DELETE"), 1, "{out}");
    let b_rows = core
        .execute_sql(&b, "SELECT id FROM docs LIMIT 10")
        .expect("b select");
    assert_eq!(b_rows.rows.len(), 1);
}

/// `operation_id` 省略（既定の `Optional` 台帳モード）では内側を評価して実行する。
#[test]
fn subquery_where_dml_without_operation_id_executes() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    run(
        &core,
        "UPDATE docs SET lang = 'xx' WHERE lang IN (SELECT lang FROM langs LIMIT 10)",
    )
    .expect("update executes");
    let rows = doc_rows(&core);
    assert!(rows.contains("xx") && rows.contains("en"), "{rows}");
    run(
        &core,
        "DELETE FROM docs WHERE EXISTS (SELECT id FROM langs LIMIT 1) AND lang = 'xx'",
    )
    .expect("delete executes");
    let rows = doc_rows(&core);
    assert!(!rows.contains("xx") && rows.contains("en"), "{rows}");
}

/// `PARTITIONED` 併用（`EXPLAIN` 経路を含む）は構文段で `42601`。
#[test]
fn subquery_where_with_partitioned_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    for sql in [
        "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) USING OPERATION_ID 'p-1' PARTITIONED",
        "UPDATE docs SET lang = 'xx' WHERE EXISTS (SELECT id FROM langs LIMIT 1) USING OPERATION_ID 'p-2' PARTITIONED",
        "EXPLAIN DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) USING OPERATION_ID 'p-3' PARTITIONED",
    ] {
        let err = run(&core, sql).expect_err("partitioned + subquery");
        assert_42601(&err);
    }
}

/// `RETURNING` の投影位置サブクエリは従来どおり `42601`。
#[test]
fn subquery_in_returning_projection_stays_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    let err = run(
        &core,
        "DELETE FROM docs WHERE lang = 'en' RETURNING (SELECT lang FROM langs LIMIT 1) USING OPERATION_ID 'r-1'",
    )
    .expect_err("returning subquery");
    assert_42601(&err);
}

/// 拡張クエリプロトコル: `$n` との併用は Parse で `42601`、`$n` なしの `USING OPERATION_ID` 付きは
/// Parse → Bind → Execute で受理され、再送は `23505`、別文の再利用は `22023`。
#[test]
fn subquery_where_dml_over_extended_protocol() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    seed(&core);
    let err = core
        .parse_sql_prepared(
            "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) AND id = $1 USING OPERATION_ID 'e-1'",
        )
        .expect_err("subquery with $n");
    assert_42601(&err);
    let exec = |sql: &str| {
        let prepared = core.parse_sql_prepared(sql)?;
        let parsed = core.bind_prepared(&prepared, &[])?;
        core.execute_parsed_in_session(&ctx(), &mut SessionState::default(), &parsed)
            .map(|o| format!("{o:?}"))
    };
    let sql =
        "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) USING OPERATION_ID 'e-2'";
    let out = exec(sql).expect("executes");
    assert_eq!(affected_tag(&out, "DELETE"), 1, "{out}");
    assert_eq!(exec(sql).expect_err("resend").wire_code(), "23505");
    let other =
        "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 5) USING OPERATION_ID 'e-2'";
    assert_eq!(exec(other).expect_err("reuse").wire_code(), "22023");
}
