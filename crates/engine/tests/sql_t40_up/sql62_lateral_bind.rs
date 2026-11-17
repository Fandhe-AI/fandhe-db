//! LATERAL 派生表の構文受理と左側の列の束縛の結合テスト（SQL-62・Issue #3321、親 #3319）。
//! ポインタ: SQL-60・SQL-61・TASK-344・RLS-10 (b)。設計記録 `docs/design/lateral-derived-table.md`。
//!
//! 外側参照の無い LATERAL は通常の派生表と同じ結果になり（L6）、左側の列を参照する形は束縛を通した
//! あと専用文言の `0A000` で止まる（実行は #3322 以降）。不正な外側参照は束縛段で拒否し、予約内部名を
//! エラー文に出さない。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
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

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn query_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    query(core, tenant, sql).expect_err("must be rejected")
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

fn render(r: &QueryResult) -> String {
    format!(
        "{:?}",
        r.rows
            .iter()
            .map(|x| (x.id, format!("{:?}", x.cells)))
            .collect::<Vec<_>>()
    )
}

fn ids(r: &QueryResult) -> Vec<u64> {
    r.rows.iter().map(|x| x.id).collect()
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query_err(core, tenant, sql).wire_code().to_string()
}

fn err_pair(core: &EngineCore, tenant: &str, sql: &str) -> (String, String) {
    let e = query_err(core, tenant, sql);
    (e.wire_code().to_string(), e.client_message())
}

/// 任意の文を実行して拒否されることを確かめる（Query 以外の outcome を返す文の入口を含む）。
fn exec_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .expect_err("must be rejected")
}

const CORRELATED_MSG: &str = "LATERAL subqueries referencing outer columns are not supported yet";

#[test]
fn uncorrelated_lateral_matches_plain_derived_table() {
    let (core, _g) = open();
    // 外側に ORDER BY を付けて順序を決定的にする。
    for (lateral, plain) in [
        (
            "SELECT * FROM LATERAL (SELECT lang FROM docs) t",
            "SELECT * FROM (SELECT lang FROM docs) t",
        ),
        (
            "SELECT d.id, t.lang FROM docs d, LATERAL (SELECT lang FROM docs WHERE lang = 'en') t ORDER BY d.id LIMIT 100",
            "SELECT d.id, t.lang FROM docs d, (SELECT lang FROM docs WHERE lang = 'en') t ORDER BY d.id LIMIT 100",
        ),
        (
            "SELECT d.id, t.lang FROM docs d CROSS JOIN LATERAL (SELECT lang FROM docs WHERE lang = 'en') t ORDER BY d.id LIMIT 100",
            "SELECT d.id, t.lang FROM docs d CROSS JOIN (SELECT lang FROM docs WHERE lang = 'en') t ORDER BY d.id LIMIT 100",
        ),
        (
            "SELECT d.id, t.lang FROM docs d JOIN LATERAL (SELECT lang FROM docs WHERE lang = 'en') t ON true ORDER BY d.id LIMIT 100",
            "SELECT d.id, t.lang FROM docs d CROSS JOIN (SELECT lang FROM docs WHERE lang = 'en') t ORDER BY d.id LIMIT 100",
        ),
        (
            "SELECT d.id, t.x FROM docs d INNER JOIN LATERAL (SELECT lang FROM docs WHERE lang = 'en') t(x) ON true ORDER BY d.id LIMIT 100",
            "SELECT d.id, t.x FROM docs d CROSS JOIN (SELECT lang FROM docs WHERE lang = 'en') t(x) ORDER BY d.id LIMIT 100",
        ),
    ] {
        for tenant in ["alice", "bob", "carol"] {
            let a = query(&core, tenant, lateral).unwrap_or_else(|e| panic!("{lateral}: {e}"));
            let b = query(&core, tenant, plain).expect(plain);
            assert_eq!(render(&a), render(&b), "{lateral} ({tenant})");
        }
    }
}

#[test]
fn uncorrelated_lateral_sees_only_the_tenants_visible_rows() {
    let (core, _g) = open();
    let sql = "SELECT * FROM LATERAL (SELECT lang FROM docs) t";
    // alice は自分の private 行を含み、carol は含まない（件数が異なる）。
    for tenant in ["alice", "carol"] {
        let base = query(&core, tenant, "SELECT * FROM docs").expect("base");
        assert_eq!(
            query(&core, tenant, sql).expect(tenant).rows.len(),
            base.rows.len()
        );
    }
    assert_ne!(
        query(&core, "alice", sql).expect("alice").rows.len(),
        query(&core, "carol", sql).expect("carol").rows.len()
    );
}

#[test]
fn trailing_semicolon_gives_the_same_result() {
    let (core, _g) = open();
    let base = "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs) t ORDER BY d.id LIMIT 100";
    let a = query(&core, "alice", base).expect("without ;");
    let b = query(&core, "alice", &format!("{base};")).expect("with ;");
    assert_eq!(ids(&a), ids(&b));
    assert!(!ids(&a).is_empty());
    // `FROM LATERAL (...) t;` と `JOIN LATERAL (...) t ON true;`（`ON true` の終端判定）も同じ。
    for base in [
        "SELECT * FROM LATERAL (SELECT lang FROM docs) t",
        "SELECT d.id FROM docs d JOIN LATERAL (SELECT lang FROM docs) t ON true",
    ] {
        let a = query(&core, "alice", base).expect(base);
        let b = query(&core, "alice", &format!("{base};")).expect(base);
        assert_eq!(render(&a), render(&b), "{base}");
    }
    // 外側参照のある形も末尾 `;` で束縛を通り、同じ専用文言で止まる。
    let c = "SELECT * FROM docs d JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true";
    assert_eq!(
        err_pair(&core, "alice", c),
        err_pair(&core, "alice", &format!("{c};"))
    );
}

// tracked: #3322, #3323
#[test]
fn correlated_forms_pass_binding_and_stop_with_the_dedicated_message() {
    let (core, _g) = open();
    for sql in [
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t(c)",
        "SELECT * FROM docs d CROSS JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t",
        "SELECT * FROM docs d JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true",
        "SELECT * FROM docs d LEFT JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true",
        "SELECT * FROM docs d LEFT OUTER JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true",
        // 非修飾の外側参照（`lang` は内側にもあるため内側優先。内側に無い `body` を使う）。
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.body) t",
        // 左側が CTE で、外側の列を完全に非修飾で参照する（`only_c` は内側の `docs` に無い）。
        "WITH c(only_c) AS (SELECT lang FROM docs) SELECT * FROM c, LATERAL (SELECT lang FROM docs WHERE lang = only_c) t",
        // AND／OR／括弧の中。
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = 'ja' AND (x.body = d.body OR x.lang = 'en')) t",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang OR x.lang = 'en') t",
        // 左側が 2 relation。
        "SELECT * FROM docs a, docs b, LATERAL (SELECT lang FROM docs x WHERE x.lang = b.lang) t",
    ] {
        for tenant in ["alice", "carol"] {
            let (c, m) = err_pair(&core, tenant, sql);
            assert_eq!(c, "0A000", "{sql}");
            assert!(m.ends_with(CORRELATED_MSG), "{sql}: {m}");
        }
    }
    // 副作用なし: 拒否の前後で件数が変わらない。
    assert_eq!(
        query(&core, "alice", "SELECT * FROM docs")
            .expect("count")
            .rows
            .len(),
        6
    );
}

/// 暗黙 rowid の `id` は内側・左側のどちらでも列として束縛できる（`42703` にしない）。
// tracked: #3322, #3323
#[test]
fn implicit_id_binds_on_inner_and_left_sides() {
    let (core, _g) = open();
    for sql in [
        // 内側の述語が内側の `id` を参照し、外側の列とも比較する。
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.id = d.lang) t",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE id = d.lang) t",
        // 左側の `id` を外側参照する。
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.id) t",
    ] {
        let (c, m) = err_pair(&core, "alice", sql);
        assert_eq!(c, "0A000", "{sql}: {m}");
        assert!(m.ends_with(CORRELATED_MSG), "{sql}: {m}");
    }
}

// tracked: #3323
#[test]
fn uncorrelated_left_join_lateral_is_a_dedicated_0a000() {
    let (core, _g) = open();
    let (c, m) = err_pair(
        &core,
        "alice",
        "SELECT * FROM docs d LEFT JOIN LATERAL (SELECT lang FROM docs) t ON true",
    );
    assert_eq!(c, "0A000");
    assert!(m.ends_with("LEFT JOIN LATERAL is not supported yet"), "{m}");
}

#[test]
fn invalid_outer_references_are_rejected_in_binding() {
    let (core, _g) = open();
    for (sql, expected) in [
        // 右側の FROM 項目・自分自身・先頭 LATERAL からの修飾参照。
        (
            "SELECT * FROM docs a, LATERAL (SELECT lang FROM docs x WHERE x.lang = c.lang) t, docs c",
            "42P01",
        ),
        (
            "SELECT * FROM docs a, LATERAL (SELECT lang FROM docs x WHERE x.lang = t.lang) t",
            "42P01",
        ),
        (
            "SELECT * FROM LATERAL (SELECT lang FROM docs x WHERE x.lang = a.lang) t, docs a",
            "42P01",
        ),
        // 左側の表が存在しない（`42P01` が `0A000` に先行する）。
        (
            "SELECT * FROM nope d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t",
            "42P01",
        ),
        // 左側に無い列。
        (
            "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.nope) t",
            "42703",
        ),
        (
            "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE nope = 'a') t",
            "42703",
        ),
        (
            "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.nope = d.lang) t",
            "42703",
        ),
    ] {
        assert_eq!(code(&core, "alice", sql), expected, "{sql}");
    }
}

#[test]
fn ambiguous_unqualified_outer_name_is_42702() {
    let (core, _g) = open();
    let sql = "WITH c(only_in_c) AS (SELECT lang FROM docs) SELECT * FROM docs a, docs b, \
               LATERAL (SELECT only_in_c FROM c WHERE only_in_c = body) t";
    assert_eq!(
        code(&core, "alice", sql),
        "42702",
        "{}",
        err_pair(&core, "alice", sql).1
    );
}

#[test]
fn binding_errors_are_identical_across_tenants_and_hide_internal_names() {
    let (core, _g) = open();
    for sql in [
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.nope) t",
        "SELECT * FROM docs a, LATERAL (SELECT lang FROM docs x WHERE x.lang = z.lang) t",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t",
    ] {
        let a = err_pair(&core, "alice", sql);
        let c = err_pair(&core, "carol", sql);
        assert_eq!(a, c, "{sql}");
        assert!(
            !a.1.contains('\u{2}') && !a.1.contains("lateral0"),
            "{sql}: {}",
            a.1
        );
    }
}

// tracked: #3322, #3323, #3324
#[test]
fn unsupported_lateral_shapes_stay_fail_closed() {
    let (core, _g) = open();
    for sql in [
        // 外側参照を置けない位置。
        "SELECT * FROM docs d, LATERAL (SELECT d.lang FROM docs x) t",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x ORDER BY d.lang) t",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang IN (SELECT lang FROM docs y WHERE y.lang = d.lang)) t",
        // 入れ子の LATERAL・RIGHT／FULL／NATURAL。
        "SELECT * FROM (SELECT lang FROM docs a, LATERAL (SELECT lang FROM docs) b) t",
        "SELECT * FROM docs d RIGHT JOIN LATERAL (SELECT lang FROM docs) t ON true",
        "SELECT * FROM docs d FULL JOIN LATERAL (SELECT lang FROM docs) t ON true",
        "SELECT * FROM docs d NATURAL JOIN LATERAL (SELECT lang FROM docs) t",
        // 本文が SELECT 以外。
        "SELECT * FROM docs d, LATERAL (VALUES (1)) t",
        "SELECT * FROM docs d, LATERAL (WITH c AS (SELECT lang FROM docs) SELECT lang FROM c) t",
        // EXPLAIN。
        "EXPLAIN SELECT * FROM docs d, LATERAL (SELECT lang FROM docs) t",
        // 別名なし（先頭以外）。
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs)",
        // 書き込みを伴う CTE は書き込みなしで拒否される。
        "WITH w AS (DELETE FROM docs WHERE lang = 'en' RETURNING id) SELECT * FROM docs d, LATERAL (SELECT lang FROM docs) t",
    ] {
        assert_eq!(code(&core, "alice", sql), "0A000", "{sql}");
    }
    assert_eq!(
        query(&core, "alice", "SELECT * FROM docs")
            .expect("count")
            .rows
            .len(),
        6
    );
}

/// 受理しない入口（cursor・COPY・CTAS・ビュー本文）は従来どおり拒否され、書き込みは起きない
/// （設計記録 L17。LATERAL を受理しない入口の `wire_code` は派生表を置いた場合と同じ）。
// tracked: #3324
#[test]
fn lateral_in_other_entry_points_is_rejected() {
    let (core, _g) = open();
    for (lateral, plain) in [
        (
            "DECLARE c CURSOR FOR SELECT * FROM LATERAL (SELECT lang FROM docs) t",
            "DECLARE c CURSOR FOR SELECT * FROM (SELECT lang FROM docs) t",
        ),
        (
            "COPY (SELECT * FROM LATERAL (SELECT lang FROM docs) t) TO STDOUT",
            "COPY (SELECT * FROM (SELECT lang FROM docs) t) TO STDOUT",
        ),
        (
            "CREATE TABLE ctas_l AS SELECT * FROM LATERAL (SELECT lang FROM docs) t",
            "CREATE TABLE ctas_p AS SELECT * FROM (SELECT lang FROM docs) t",
        ),
        (
            "CREATE VIEW v_l AS SELECT * FROM LATERAL (SELECT lang FROM docs) t",
            "CREATE VIEW v_p AS SELECT * FROM (SELECT lang FROM docs) t",
        ),
    ] {
        let l = exec_err(&core, "alice", lateral).wire_code().to_string();
        let p = exec_err(&core, "alice", plain).wire_code().to_string();
        assert_eq!(l, p, "{lateral}");
    }
    assert!(query(&core, "alice", "SELECT * FROM v_l").is_err());
    assert!(query(&core, "alice", "SELECT * FROM ctas_l").is_err());
}
