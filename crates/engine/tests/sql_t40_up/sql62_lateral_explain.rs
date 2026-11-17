//! LATERAL 派生表の `EXPLAIN` の結合テスト（SQL-62・Issue #3324、親 #3319）。
//! ポインタ: SQL-60・SQL-61・SQL-62 (7)・TASK-344・RLS-10。設計記録 `docs/design/lateral-derived-table.md`（L15・L17）。
//! 出力語彙は閉じており（名前・値・行数・予約内部名を出さない）、全テナントで同一になる。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, lang: &str, visibility: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text("b".to_string()),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// alice: public 3 件（ja 2・en 1）・private 1 件（ja）。bob: public 1 件。carol: public 1 件。
fn open() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-lateral-bind");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (id, tenant, lang, vis) in [
        (1u64, "alice", "ja", Visibility::Public),
        (2, "alice", "ja", Visibility::Public),
        (3, "alice", "en", Visibility::Public),
        (4, "alice", "ja", Visibility::Private),
        (5, "bob", "ja", Visibility::Public),
        (7, "carol", "ja", Visibility::Public),
    ] {
        insert_row(&storage, tenant, id, lang, vis);
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn explain_lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    match run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlOutcome::Explain(r) => r
            .rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(Cell::Json(s) | Cell::Text(s)) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain for {sql}, got {other:?}"),
    }
}

/// 出力に出してはならない語（表名・別名・列名・予約内部名・合成名）が無いこと。
fn assert_closed_vocabulary(lines: &[String], sql: &str) {
    let all = lines.join("\n");
    for banned in [
        "docs", "lang", "body", "tag", "\u{2}", "lateral0", "?derived",
    ] {
        assert!(!all.contains(banned), "{sql}: leaked {banned:?} in {all}");
    }
    // 別名（d・x・t）が単語として現れないこと。
    for token in all.split(|c: char| !c.is_alphanumeric() && c != '_') {
        assert!(
            !["d", "x", "t"].contains(&token),
            "{sql}: alias {token:?} in {all}"
        );
    }
}

const CORRELATED: &str =
    "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t";

/// 外側参照のある LATERAL の各結合形（コンマ・CROSS・INNER ON true・LEFT ON true）。
fn correlated_forms() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "comma",
            "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t",
        ),
        (
            "cross",
            "SELECT d.id FROM docs d CROSS JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t",
        ),
        (
            "inner",
            "SELECT d.id FROM docs d INNER JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true",
        ),
        (
            "left",
            "SELECT d.id FROM docs d LEFT JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true",
        ),
    ]
}

#[test]
fn correlated_lateral_shows_per_outer_row_reevaluation_in_every_join_form() {
    let (core, _g) = open();
    for (form, body) in correlated_forms() {
        let sql = format!("EXPLAIN {body}");
        let lines = explain_lines(&core, "alice", &sql);
        assert!(
            lines.contains(&"join_method: lateral".to_string()),
            "{form}: {lines:?}"
        );
        assert!(
            lines.contains(&"reevaluation: per_outer_row".to_string()),
            "{form}: {lines:?}"
        );
        let expected_type = if form == "left" { "left" } else { "inner" };
        assert!(
            lines.contains(&format!("join_type: {expected_type}")),
            "{form}: {lines:?}"
        );
        assert_closed_vocabulary(&lines, &sql);
        // 実行できる文であり、EXPLAIN は書き込み・結果行を作らない（表の件数は不変）。
        assert!(run(&core, "alice", body).is_ok(), "{form}");
    }
}

#[test]
fn uncorrelated_lateral_is_a_plain_derived_table_without_reevaluation() {
    let (core, _g) = open();
    let sql =
        "EXPLAIN SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = 'ja') t";
    let lines = explain_lines(&core, "alice", sql);
    assert!(
        !lines.iter().any(|l| l.starts_with("reevaluation")),
        "{lines:?}"
    );
    assert!(
        !lines.contains(&"join_method: lateral".to_string()),
        "{lines:?}"
    );
    assert_closed_vocabulary(&lines, sql);
}

#[test]
fn explain_output_is_identical_for_every_tenant() {
    let (core, _g) = open();
    for (form, body) in correlated_forms() {
        let sql = format!("EXPLAIN {body}");
        let a = explain_lines(&core, "alice", &sql);
        let b = explain_lines(&core, "bob", &sql);
        let c = explain_lines(&core, "carol", &sql);
        assert_eq!(a, b, "{form}");
        assert_eq!(a, c, "{form}");
    }
}

#[test]
fn explain_options_accept_lateral_without_leaking_names() {
    let (core, _g) = open();
    for prefix in [
        "EXPLAIN (COSTS OFF)",
        "EXPLAIN (VERBOSE)",
        "EXPLAIN VERBOSE",
        "EXPLAIN (FORMAT JSON)",
        "EXPLAIN ANALYZE",
    ] {
        let sql = format!("{prefix} {CORRELATED}");
        let lines = explain_lines(&core, "alice", &sql);
        let all = lines.join("\n");
        assert!(
            all.contains("reevaluation: per_outer_row")
                || all.contains("\"reevaluation\": \"per_outer_row\""),
            "{sql}: {all}"
        );
        assert!(all.contains("lateral"), "{sql}: {all}");
        // VERBOSE の `output:` は投影の公開名（`id`）だけ。表名・別名・予約内部名は出ない。
        for banned in ["docs", "\u{2}", "lateral0", "?derived"] {
            assert!(!all.contains(banned), "{sql}: leaked {banned:?} in {all}");
        }
    }
}

#[test]
fn explain_analyze_executes_with_the_callers_visibility_only() {
    let (core, _g) = open();
    let sql = format!("EXPLAIN ANALYZE {CORRELATED}");
    let rows_of = |tenant: &str| -> String {
        explain_lines(&core, tenant, &sql)
            .into_iter()
            .find(|l| l.starts_with("actual_rows:"))
            .unwrap_or_else(|| panic!("no actual_rows for {tenant}"))
    };
    // alice は自分の private 行を含めて 26 件、carol は含まない 25 件（可視行だけで評価される）。
    assert_eq!(rows_of("alice"), "actual_rows: 26");
    assert_ne!(rows_of("carol"), rows_of("alice"));
}

#[test]
fn unknown_columns_fail_like_execution_and_without_leaks() {
    let (core, _g) = open();
    let body = "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.nope) t";
    let explained = run(&core, "alice", &format!("EXPLAIN {body}")).expect_err("explain");
    let executed = run(&core, "alice", body).expect_err("execute");
    assert_eq!(explained.wire_code(), executed.wire_code());
    let other = run(&core, "carol", &format!("EXPLAIN {body}")).expect_err("explain");
    assert_eq!(
        (explained.wire_code(), explained.client_message()),
        (other.wire_code(), other.client_message())
    );
    assert!(!explained.client_message().contains('\u{2}'));
}

#[test]
fn explain_with_params_stays_rejected_at_parse() {
    let (core, _g) = open();
    let sql =
        "EXPLAIN SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = $1) t";
    let err = core.parse_sql_prepared(sql).expect_err("must be rejected");
    assert_eq!(err.wire_code(), "42601");
}
