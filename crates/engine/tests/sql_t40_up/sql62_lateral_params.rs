//! LATERAL 派生表の本文の WHERE に置いた `$n`（拡張クエリ。Parse／Describe／Bind）の結合テスト
//! （SQL-62・Issue #3324、親 #3319）。ポインタ: SQL-60・SQL-61・SQL-62 (7)・TASK-344・WIRE-12・RLS-10。
//! 設計記録 `docs/design/lateral-derived-table.md`・`docs/design/wire-extended-query-param-binding.md`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::params::PreparedParamType;
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
            ColumnDef::new("n", ColumnType::Integer, false),
            ColumnDef::new("dt", ColumnType::Date, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, lang: &str, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Integer(i32::try_from(id).expect("small id")),
            Value::Date(0),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// alice: public 3 件（id 1・2 が ja、3 が en）・private 1 件（id 4・ja）。bob: id 5（ja）。carol: id 7（ja）。
fn open() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-lateral-params");
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

fn simple(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(r) => Ok(r),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
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

/// Parse → Describe → Bind → Execute（拡張クエリの経路）の結果。
fn prepared(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    values: &[&str],
) -> Result<QueryResult, SqlSurfaceError> {
    let prepared = core.parse_sql_prepared(sql)?;
    let mut session = SessionState::default();
    let described = core.describe_prepared_in_session(&session, &prepared)?;
    let bound = core.bind_prepared(
        &prepared,
        &values
            .iter()
            .map(|v| Some(v.as_bytes().to_vec()))
            .collect::<Vec<_>>(),
    )?;
    match core.execute_parsed_in_session(&ctx(tenant), &mut session, &bound)? {
        SqlOutcome::Query(r) => {
            // Describe が公告した列と実行結果の列は一致する。
            assert_eq!(described.map(|c| c.len()), Some(r.columns.len()), "{sql}");
            Ok(r)
        }
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn parse_code(core: &EngineCore, sql: &str) -> String {
    core.parse_sql_prepared(sql)
        .expect_err(sql)
        .wire_code()
        .to_string()
}

const SELECT_HEAD: &str =
    "SELECT d.id, t.lang FROM docs d, LATERAL (SELECT lang FROM docs x WHERE ";
const SELECT_TAIL: &str = " ORDER BY lang LIMIT 2) t ORDER BY d.id, t.lang LIMIT 100";

#[test]
fn text_params_in_the_lateral_body_match_the_literal_form() {
    let (core, _g) = open();
    for (predicate, literal_predicate, value) in [
        (
            "x.lang = d.lang AND x.lang = $1",
            "x.lang = d.lang AND x.lang = 'ja'",
            "ja",
        ),
        (
            "x.lang = d.lang AND x.lang >= $1",
            "x.lang = d.lang AND x.lang >= 'ja'",
            "ja",
        ),
        (
            "x.lang = d.lang AND x.lang < $1",
            "x.lang = d.lang AND x.lang < 'jb'",
            "jb",
        ),
        (
            "lang = $1 AND x.lang = d.lang",
            "lang = 'ja' AND x.lang = d.lang",
            "ja",
        ),
        ("x.lang = $1", "x.lang = 'en'", "en"),
    ] {
        let sql = format!("{SELECT_HEAD}{predicate}{SELECT_TAIL}");
        let literal = format!("{SELECT_HEAD}{literal_predicate}{SELECT_TAIL}");
        for tenant in ["alice", "bob", "carol"] {
            let a =
                prepared(&core, tenant, &sql, &[value]).unwrap_or_else(|e| panic!("{sql}: {e}"));
            let b = simple(&core, tenant, &literal).unwrap_or_else(|e| panic!("{literal}: {e}"));
            assert_eq!(render(&a), render(&b), "{sql} ({tenant})");
        }
    }
    // 非空の結果であること（比較が空集合同士で成立していない）。
    let sql = format!("{SELECT_HEAD}x.lang = d.lang AND x.lang = $1{SELECT_TAIL}");
    assert!(!prepared(&core, "alice", &sql, &["ja"])
        .expect("alice")
        .rows
        .is_empty());
}

#[test]
fn param_slot_type_follows_the_inner_relations_column() {
    let (core, _g) = open();
    let sql = format!("{SELECT_HEAD}x.lang = d.lang AND x.lang = $1{SELECT_TAIL}");
    let p = core.parse_sql_prepared(&sql).expect("parse");
    assert_eq!(p.param_count(), 1);
    assert!(matches!(
        p.param_types(),
        [PreparedParamType::Column(ColumnMeta::Scalar {
            ty: ColumnType::Text,
            ..
        })]
    ));
}

#[test]
fn params_in_other_positions_stay_rejected() {
    let (core, _g) = open();
    for sql in [
        // 外側の WHERE・投影・ON・LIMIT。
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t WHERE d.lang = $1",
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t WHERE lang = $1",
        "SELECT $1 FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t",
        "SELECT d.id FROM docs d LEFT JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t ON t.lang = $1",
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) t LIMIT $1",
        // 外側参照の列への比較・左辺が `$n`。
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE d.lang = $1) t",
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE $1 = x.lang) t",
        // OR・括弧・キャスト・BETWEEN・整数列（派生表の本文は数値比較を持てない）。
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang OR x.lang = $1) t",
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE (x.lang = $1)) t",
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = $1::text) t",
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang BETWEEN $1 AND $2) t",
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.n >= $1) t",
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang <> $1) t",
        // 部分クエリの内側・LATERAL を 2 つ持つ文・LATERAL でない派生表。
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang IN (SELECT lang FROM docs y WHERE y.lang = $1)) t",
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = $1) t, LATERAL (SELECT lang FROM docs y WHERE y.lang = d.lang) u",
        "SELECT * FROM (SELECT lang FROM docs WHERE lang = $1) t",
    ] {
        assert_eq!(parse_code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn params_do_not_reveal_whether_the_inner_relation_exists() {
    let (core, _g) = open();
    // 内側の表が無い場合は、リテラルで書いた同じ文と同じ `wire_code`（存在の有無を応答で変えない）。
    let with_param = "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM nope x WHERE x.lang = d.lang AND x.lang = $1) t";
    let literal = "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM nope x WHERE x.lang = d.lang AND x.lang = 'ja') t";
    assert_eq!(
        parse_code(&core, with_param),
        simple(&core, "alice", literal)
            .expect_err("literal")
            .wire_code()
    );
}

#[test]
fn params_on_non_text_columns_are_rejected_and_real_literals_stay_validated() {
    let (core, _g) = open();
    // DATE 列への `$n` は Parse で固定 `42601`（Describe 用ダミー値が型検証に当たる列は受理しない）。
    let on_date = "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang AND x.dt = $1) t";
    assert_eq!(parse_code(&core, on_date), "42601");
    // 同じ文に実リテラルの不正な日付があっても、`$n` の存在で検証が省かれない
    // （リテラルだけの文と同じ `wire_code`・同じ段で拒否される）。
    let bogus = "x.dt = '2024-13-45'";
    let with_param = format!(
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang AND x.lang = $1 AND {bogus}) t"
    );
    let literal = format!(
        "SELECT d.id FROM docs d, LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang AND x.lang = 'ja' AND {bogus}) t"
    );
    let expected = simple(&core, "alice", &literal)
        .expect_err(&literal)
        .wire_code();
    // Parse か Describe のどちらかで同じコードの拒否になる（Bind まで遅延させない）。
    let got = match core.parse_sql_prepared(&with_param) {
        Err(e) => e.wire_code(),
        Ok(p) => core
            .describe_prepared_in_session(&SessionState::default(), &p)
            .expect_err(&with_param)
            .wire_code(),
    };
    assert_eq!(got, expected, "{with_param}");
}
