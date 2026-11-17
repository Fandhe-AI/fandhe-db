//! LATERAL 派生表を本文に持つビューの結合テスト（SQL-62・Issue #3324、親 #3319）。
//! ポインタ: SQL-60・SQL-61・SQL-62 (7)・TASK-344・TABLE-18・RLS-10。設計記録
//! `docs/design/lateral-derived-table.md`・`docs/design/create-view.md`。
//!
//! 受理するのは「左側を持つ LATERAL を含む JOIN 本文（`LIMIT` 必須）」だけ。参照者自身の `PolicyContext`・
//! 同じ `read_txn` で評価され、作成者の可視性は引き継がれない（RLS-10 (b)）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 外側の表。
const OUTER: &str = "docs";
/// LATERAL の内側だけが読む表（ビューの依存関係の検査に使う）。
const INNER: &str = "other";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
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

fn insert_row(
    storage: &Storage,
    table: &str,
    tenant: &str,
    id: u64,
    lang: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text("b".to_string()),
        ],
        &OperationId::parse(&format!("seed-{table}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// docs: alice は public 2 件（ja）・private 1 件（ja）、bob は public 1 件（en）、carol は private 1 件（ja）。
/// other: alice は private 1 件（ja）、bob は public 1 件（ja）、carol は public 1 件（en）・private 1 件（ja）。
/// public は全テナントに見え、private は所有者だけに見える。
fn open() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-lateral-view");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(OUTER)).expect("create docs");
    storage.create_table(&schema(INNER)).expect("create other");
    for (id, tenant, lang, vis) in [
        (1u64, "alice", "ja", Visibility::Public),
        (2, "alice", "ja", Visibility::Public),
        (3, "alice", "ja", Visibility::Private),
        (4, "bob", "en", Visibility::Public),
        (5, "carol", "ja", Visibility::Private),
    ] {
        insert_row(&storage, OUTER, tenant, id, lang, vis);
    }
    for (id, tenant, lang, vis) in [
        (11u64, "alice", "ja", Visibility::Private),
        (12, "bob", "ja", Visibility::Public),
        (13, "carol", "en", Visibility::Public),
        (14, "carol", "ja", Visibility::Private),
    ] {
        insert_row(&storage, INNER, tenant, id, lang, vis);
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ddl(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(r) => Ok(r),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn code(result: Result<impl std::fmt::Debug, SqlSurfaceError>, sql: &str) -> String {
    result.expect_err(sql).wire_code().to_string()
}

/// 行ごとのセル（行 ID は結合結果では意味を持たないため比較に含めない）。
fn cells(r: &QueryResult) -> Vec<Vec<Cell>> {
    r.rows.iter().map(|x| x.cells.clone()).collect()
}

/// 内側が別の表（other）の LATERAL を持つ JOIN 本文。
const BODY: &str = "SELECT d.id AS did, t.lang AS tl FROM docs d, LATERAL (SELECT lang FROM other x WHERE x.lang = d.lang ORDER BY lang LIMIT 2) t LIMIT 100";
/// 参照時に同じ結果になる、ビューを使わない文。
const DIRECT: &str = "SELECT d.id AS did, t.lang AS tl FROM docs d, LATERAL (SELECT lang FROM other x WHERE x.lang = d.lang ORDER BY lang LIMIT 2) t ORDER BY did, tl LIMIT 100";

#[test]
fn view_with_a_lateral_body_returns_each_referencers_visible_rows_only() {
    let (core, _g) = open();
    // 作成者は carol（作成者の可視性は引き継がれない）。
    ddl(&core, "carol", &format!("CREATE VIEW v_lat AS {BODY}")).expect("create");
    let mut seen = Vec::new();
    for tenant in ["alice", "bob", "carol"] {
        let via_view = query(
            &core,
            tenant,
            "SELECT * FROM v_lat ORDER BY did, tl LIMIT 100",
        )
        .unwrap_or_else(|e| panic!("{tenant}: {e}"));
        let direct = query(&core, tenant, DIRECT).unwrap_or_else(|e| panic!("{tenant}: {e}"));
        assert_eq!(cells(&via_view), cells(&direct), "{tenant}");
        assert!(!via_view.rows.is_empty(), "{tenant}");
        seen.push(cells(&via_view));
    }
    // 可視行が異なる alice・carol で結果が異なる（作成者 carol の可視性ではない）。
    assert_ne!(seen.first(), seen.get(2));
}

#[test]
fn view_over_lateral_body_supports_column_aliases_and_outer_where() {
    let (core, _g) = open();
    ddl(&core, "alice", &format!("CREATE VIEW v_lat AS {BODY}")).expect("create");
    ddl(
        &core,
        "alice",
        &format!("CREATE VIEW v_alias (a, b) AS {BODY}"),
    )
    .expect("create alias");
    let filtered = query(
        &core,
        "alice",
        "SELECT * FROM v_lat WHERE tl = 'ja' LIMIT 100",
    )
    .expect("where");
    assert!(!filtered.rows.is_empty());
    assert!(filtered
        .rows
        .iter()
        .all(|r| r.cells.get(1) == Some(&Cell::Text("ja".to_string()))));
    let aliased = query(
        &core,
        "alice",
        "SELECT a, b FROM v_alias ORDER BY a, b LIMIT 100",
    )
    .expect("alias");
    assert_eq!(aliased.columns.len(), 2);
    assert!(!aliased.rows.is_empty());
}

#[test]
fn dropping_a_table_read_only_inside_the_lateral_body_is_refused() {
    let (core, _g) = open();
    ddl(&core, "alice", &format!("CREATE VIEW v_lat AS {BODY}")).expect("create");
    // `other` は LATERAL の内側だけが読む表。外側の `docs` と同じく依存として扱われる。
    for table in [INNER, OUTER] {
        let sql = format!("DROP TABLE {table}");
        assert_eq!(code(ddl(&core, "alice", &sql), &sql), "2BP01", "{sql}");
    }
    // 拒否された後もビューは参照できる。
    assert!(query(&core, "alice", "SELECT * FROM v_lat LIMIT 100").is_ok());
}

#[test]
fn create_or_replace_revalidates_the_lateral_body() {
    let (core, _g) = open();
    ddl(&core, "alice", &format!("CREATE VIEW v_lat AS {BODY}")).expect("create");
    let missing = BODY.replace("other x", "nope x");
    let sql = format!("CREATE OR REPLACE VIEW v_lat AS {missing}");
    assert_eq!(code(ddl(&core, "alice", &sql), &sql), "42P01");
    // 失敗した置換は元のビューを変えない。
    assert!(query(&core, "alice", "SELECT * FROM v_lat LIMIT 100").is_ok());
    // 内側の表を `docs` に替えた本文へは置換できる。
    let replaced = BODY.replace("other x", "docs x");
    ddl(
        &core,
        "alice",
        &format!("CREATE OR REPLACE VIEW v_lat AS {replaced}"),
    )
    .expect("replace");
    assert!(query(&core, "alice", "SELECT * FROM v_lat LIMIT 100").is_ok());
}

#[test]
fn body_shape_errors_are_fixed_and_leave_no_view_behind() {
    let (core, _g) = open();
    // 非集計 JOIN 本文は `LIMIT` が必須（本文の暗黙上限を持たない）。
    let no_limit = BODY.trim_end_matches(" LIMIT 100");
    let sql = format!("CREATE VIEW v_nolimit AS {no_limit}");
    assert_eq!(code(ddl(&core, "alice", &sql), &sql), "42601");
    // 内側の表が無い本文は `42P01`（リテラルだけの文と同じ）。
    let sql = format!(
        "CREATE VIEW v_nope AS {}",
        BODY.replace("other x", "nope x")
    );
    assert_eq!(code(ddl(&core, "alice", &sql), &sql), "42P01");
    for name in ["v_nolimit", "v_nope"] {
        let sql = format!("SELECT * FROM {name} LIMIT 1");
        assert_eq!(code(query(&core, "alice", &sql), &sql), "42P01", "{name}");
    }
}

#[test]
fn other_view_forms_with_lateral_keep_their_codes() {
    let (core, _g) = open();
    // 単一 FROM の LATERAL・LATERAL でない派生表は、従来どおり派生表を置いた場合と同じコードで拒否される。
    for (lateral, plain) in [
        (
            "CREATE VIEW v1 AS SELECT * FROM LATERAL (SELECT lang FROM docs) t LIMIT 5",
            "CREATE VIEW v2 AS SELECT * FROM (SELECT lang FROM docs) t LIMIT 5",
        ),
        // 具体化ビューの本文は LATERAL を受理しない。
        (
            "CREATE MATERIALIZED VIEW m1 AS SELECT d.id FROM docs d, LATERAL (SELECT lang FROM other x WHERE x.lang = d.lang) t",
            "CREATE MATERIALIZED VIEW m2 AS SELECT d.id FROM docs d, (SELECT lang FROM other x) t",
        ),
    ] {
        let l = code(ddl(&core, "alice", lateral), lateral);
        let p = code(ddl(&core, "alice", plain), plain);
        assert_eq!(l, p, "{lateral}");
    }
    for name in ["v1", "v2", "m1", "m2"] {
        let sql = format!("SELECT * FROM {name} LIMIT 1");
        assert_eq!(code(query(&core, "alice", &sql), &sql), "42P01", "{name}");
    }
}

// 評価後射影形ビューの EXPLAIN は、集計・LIMIT・JOIN 本文のビューと同じく固定 `42601` のまま
// （LATERAL を本文に持つビューも同じ。ビュー経由の EXPLAIN は本 Issue の対象外）。
// intentional: docs/design/lateral-derived-table.md
#[test]
fn explain_through_a_lateral_body_view_stays_rejected() {
    let (core, _g) = open();
    ddl(&core, "alice", &format!("CREATE VIEW v_lat AS {BODY}")).expect("create");
    let sql = "EXPLAIN SELECT * FROM v_lat";
    let lateral = code(query(&core, "alice", sql), sql);
    ddl(
        &core,
        "alice",
        "CREATE VIEW v_join AS SELECT d.id AS did FROM docs d JOIN other o ON d.lang = o.lang LIMIT 100",
    )
    .expect("plain join view");
    let sql_plain = "EXPLAIN SELECT * FROM v_join";
    assert_eq!(lateral, code(query(&core, "alice", sql_plain), sql_plain));
}

#[test]
fn lateral_view_is_usable_through_the_extended_protocol() {
    let (core, _g) = open();
    ddl(&core, "alice", &format!("CREATE VIEW v_lat AS {BODY}")).expect("create");
    let p = core
        .parse_sql_prepared("SELECT * FROM v_lat ORDER BY did, tl LIMIT 100")
        .expect("parse");
    assert_eq!(p.param_count(), 0);
    let session = SessionState::default();
    let columns = core
        .describe_prepared_in_session(&session, &p)
        .expect("describe")
        .expect("columns");
    assert_eq!(columns.len(), 2);
}
