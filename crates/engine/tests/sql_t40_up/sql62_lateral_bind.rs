//! LATERAL 派生表の構文受理と左側の列の束縛の結合テスト（SQL-62・Issue #3321、親 #3319）。
//! ポインタ: SQL-60・SQL-61・TASK-344・RLS-10 (b)。設計記録 `docs/design/lateral-derived-table.md`。
//!
//! 外側参照の無い LATERAL は通常の派生表と同じ結果になり（L6）、左側の列を参照する INNER／CROSS／
//! コンマの形は #3322 で外側の行ごとに再評価して実行する（結果は `sql62_lateral_exec.rs` が固定。
//! 本ファイルは束縛を通ったあとの件数で確かめる）。`LEFT JOIN LATERAL` は #3323 で実行でき、RIGHT／FULL の
//! 左側参照の `42P10` は `sql62_lateral_left.rs` が固定する。
//! 不正な外側参照は束縛段で拒否し、予約内部名をエラー文に出さない。

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
    // 外側参照のある形（#3322 で実行できる）も、末尾 `;` の有無で結果が一致する。
    let c = "SELECT d.id FROM docs d JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true ORDER BY d.id";
    let a = query(&core, "alice", c).expect(c);
    let b = query(&core, "alice", &format!("{c};")).expect(c);
    assert_eq!(render(&a), render(&b));
    assert_eq!(a.rows.len(), 26);
}

/// 左側の列を参照する INNER／CROSS／コンマの形は実行でき、件数は PostgreSQL の意味論どおり
/// （alice は ja 5 行・en 1 行、carol は private 行が見えず ja 4 行・en 1 行）。
#[test]
fn correlated_forms_execute_with_the_expected_row_counts() {
    let (core, _g) = open();
    for (sql, alice, carol) in [
        (
            "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t",
            26usize,
            17usize,
        ),
        (
            "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t(c)",
            26,
            17,
        ),
        (
            "SELECT * FROM docs d CROSS JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t",
            26,
            17,
        ),
        (
            "SELECT * FROM docs d JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true",
            26,
            17,
        ),
        // 非修飾の外側参照（`lang` は内側にもあるため内側優先。内側に無い `body` を使う）。`body` は
        // 全行 `b` で `lang` と一致しないため 0 行。
        (
            "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.body) t",
            0,
            0,
        ),
        // 左側が CTE で、外側の列を完全に非修飾で参照する（`only_c` は内側の `docs` に無い）。
        (
            "WITH c(only_c) AS (SELECT lang FROM docs) SELECT * FROM c, LATERAL (SELECT lang FROM docs WHERE lang = only_c) t",
            26,
            17,
        ),
        // AND／OR／括弧の中。
        (
            "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = 'ja' AND (x.body = d.body OR x.lang = 'en')) t",
            30,
            20,
        ),
        (
            "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang OR x.lang = 'en') t",
            31,
            21,
        ),
        // 左側が 2 relation。
        (
            "SELECT * FROM docs a, docs b, LATERAL (SELECT lang FROM docs x WHERE x.lang = b.lang) t",
            156,
            85,
        ),
    ] {
        for (tenant, expected) in [("alice", alice), ("carol", carol)] {
            let r = query(&core, tenant, sql).unwrap_or_else(|e| panic!("{sql} ({tenant}): {e}"));
            assert_eq!(r.rows.len(), expected, "{sql} ({tenant})");
        }
    }
    // 副作用なし: 実行の前後で件数が変わらない。
    assert_eq!(
        query(&core, "alice", "SELECT * FROM docs")
            .expect("count")
            .rows
            .len(),
        6
    );
}

/// 左側を参照する `LEFT JOIN LATERAL ... ON true` は束縛を通って実行され、全外側行が内側に一致する形では
/// `JOIN LATERAL ... ON true` と同じ結果になる（#3323。NULL 補完の値は `sql62_lateral_left.rs` が固定）。
#[test]
fn correlated_left_join_lateral_executes_like_the_inner_form_when_every_row_matches() {
    let (core, _g) = open();
    let inner = "SELECT * FROM docs d JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true";
    for sql in [
        "SELECT * FROM docs d LEFT JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true",
        "SELECT * FROM docs d LEFT OUTER JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true",
    ] {
        for tenant in ["alice", "carol"] {
            let l = query(&core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
            let i = query(&core, tenant, inner).expect("inner form");
            assert!(!l.rows.is_empty(), "{sql}");
            assert_eq!(l.rows.len(), i.rows.len(), "{tenant}: {sql}");
        }
    }
}

/// 暗黙 rowid の `id` は内側・左側のどちらでも列として束縛できる（`42703` にしない）。型の合わない比較
/// （`bigint` と `text`）は内部エラーにならず、テナントに依らない `42883` で拒否される（PostgreSQL 17.12 の
/// 実機で同じ型の組が `42883` になることを確認済み）。
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
        let a = err_pair(&core, "alice", sql);
        assert_eq!(a, err_pair(&core, "carol", sql), "{sql}");
        assert_eq!(a.0, "42883", "{sql}: {}", a.1);
        assert!(!a.1.contains('\u{2}'), "{sql}: {}", a.1);
    }
    // 同じ型どうし（`id` 対 `id`）の比較は実行できる。
    let r = query(
        &core,
        "alice",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.id = d.id) t",
    )
    .expect("id = id");
    assert_eq!(r.rows.len(), 6);
}

/// 左側を参照しない `LEFT JOIN LATERAL` は通常の派生表の LEFT JOIN として実行され、内側が空なら外側の全行が
/// NULL 補完で残る（#3323。L6）。
#[test]
fn uncorrelated_left_join_lateral_executes_as_a_plain_left_join() {
    let (core, _g) = open();
    let outer = query(&core, "alice", "SELECT * FROM docs d")
        .expect("outer")
        .rows
        .len();
    let full = query(
        &core,
        "alice",
        "SELECT * FROM docs d LEFT JOIN LATERAL (SELECT lang FROM docs) t ON true",
    )
    .expect("left join");
    assert_eq!(full.rows.len(), outer * outer);
    let empty = query(
        &core,
        "alice",
        "SELECT * FROM docs d LEFT JOIN LATERAL (SELECT lang FROM docs WHERE lang = 'zz') t ON true",
    )
    .expect("empty inner");
    assert_eq!(empty.rows.len(), outer);
    assert!(empty
        .rows
        .iter()
        .all(|r| matches!(r.cells.last(), Some(engine::sql::exec::Cell::Null))));
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
        "SELECT * FROM docs d RIGHT JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON true",
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

/// LATERAL の拒否固定の共通検査: 全形が `0A000` で、拒否の後も表の内容が変わらない。
fn assert_all_0a000(sqls: &[&str]) {
    let (core, _g) = open();
    for sql in sqls {
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

// 外側参照は本文の WHERE の連言にだけ置ける（L5）。投影・ORDER BY・部分クエリの内側は固定 `0A000`。
// intentional: docs/design/lateral-derived-table.md
#[test]
fn outer_references_outside_where_stay_fail_closed() {
    assert_all_0a000(&[
        "SELECT * FROM docs d, LATERAL (SELECT d.lang FROM docs x) t",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x ORDER BY d.lang) t",
        "SELECT * FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang IN (SELECT lang FROM docs y WHERE y.lang = d.lang)) t",
    ]);
}

// 派生表の本文は単一の関係に限る（JOIN を持つ本文が無い）ため、派生表の本文の中の LATERAL は固定 `0A000`。
// intentional: docs/design/lateral-derived-table.md
#[test]
fn lateral_inside_a_derived_table_body_stays_fail_closed() {
    assert_all_0a000(&[
        "SELECT * FROM (SELECT lang FROM docs a, LATERAL (SELECT lang FROM docs) b) t",
    ]);
}

// LATERAL の本文は SELECT に限る（VALUES・WITH は単一の関係へ写せない）。
// intentional: docs/design/lateral-derived-table.md
#[test]
fn lateral_bodies_other_than_select_stay_fail_closed() {
    assert_all_0a000(&[
        "SELECT * FROM docs d, LATERAL (VALUES (1)) t",
        "SELECT * FROM docs d, LATERAL (WITH c AS (SELECT lang FROM docs) SELECT lang FROM c) t",
    ]);
}

// 別名なしの LATERAL は、複数リレーションの FROM では公開名が内部名になるため受け付けない。
// intentional: docs/design/lateral-derived-table.md
#[test]
fn lateral_without_alias_stays_fail_closed() {
    assert_all_0a000(&["SELECT * FROM docs d, LATERAL (SELECT lang FROM docs)"]);
}

// 書き込みを伴う CTE は、書き込みを発生させないよう書き込みなしで拒否される。
// intentional: docs/design/lateral-derived-table.md
#[test]
fn lateral_with_data_modifying_cte_stays_fail_closed() {
    assert_all_0a000(&[
        "WITH w AS (DELETE FROM docs WHERE lang = 'en' RETURNING id) SELECT * FROM docs d, LATERAL (SELECT lang FROM docs) t",
    ]);
}

/// 受理しない入口（cursor・COPY・CTAS・ビュー本文）は従来どおり拒否され、書き込みは起きない
/// （設計記録 L17。LATERAL を受理しない入口の `wire_code` は派生表を置いた場合と同じ）。
/// ビュー本文は、左側を持つ JOIN 本文だけを受理する（`sql62_lateral_view.rs`）。単一 FROM の LATERAL は
/// 従来どおり拒否する。
// intentional: docs/design/lateral-derived-table.md
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
