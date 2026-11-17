//! `LEFT JOIN LATERAL` の外側保持と NULL 補完・ON 条件・RIGHT／FULL の結合テスト（SQL-62・Issue #3323、
//! 親 #3319）。ポインタ: SQL-60・SQL-61・SQL-62・TASK-344・RLS-10。設計記録
//! `docs/design/lateral-derived-table.md`（L1・L2・L7〜L16）。
//!
//! 期待値は本テストが固定値で持つ。alice の可視行（6 行）と carol の可視行（alice の private 行を除く 5 行）と
//! 同じ表を PostgreSQL 17.12 の実機へ作り、各問い合わせの結果行と SQLSTATE を照合済み（内側には
//! `ORDER BY` を付け、結果が一意に決まる形だけを使う）。PostgreSQL が受理するが本実装が受理しない形
//! （相関 LATERAL の USING／NATURAL・行ごとの集計）は固定文言の `0A000` のままにする（受け皿 Issue は
//! 未起票のため追跡注記は付けず、各テストの本文に理由を書く）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::correlated_limits::CorrelatedLimits;
use engine::join_limits::JoinSizeLimits;
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

const TABLE: &str = "docs";
const REL: usize = 8;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("tag", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(
    storage: &Storage,
    tenant: &str,
    id: u64,
    lang: &str,
    body: &str,
    tag: Option<&str>,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
            tag.map_or(Value::Null, |t| Value::Text(t.to_string())),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// 公開行は全テナントから見える。alice だけが private 行（id 4）も見える
/// （`sql62_lateral_exec.rs` と同じ 6 行）。
fn seed(storage: &Storage) {
    for (id, tenant, lang, body, tag, vis) in [
        (1u64, "alice", "ja", "b1", Some("t-a"), Visibility::Public),
        (2, "alice", "ja", "b2", None, Visibility::Public),
        (3, "alice", "en", "b3", Some("t-a"), Visibility::Public),
        (4, "alice", "ja", "b9", Some("t-b"), Visibility::Private),
        (5, "bob", "ja", "b5", Some("t-a"), Visibility::Public),
        (7, "carol", "ja", "b7", None, Visibility::Public),
    ] {
        insert_row(storage, tenant, id, lang, body, tag, vis);
    }
}

fn open_with(f: impl FnOnce(EngineCore) -> EngineCore) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-lateral-left");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage);
    (
        f(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

fn open() -> (EngineCore, CleanupGuard) {
    open_with(|c| c)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn err_pair(core: &EngineCore, tenant: &str, sql: &str) -> (String, String) {
    let e = run(core, tenant, sql).expect_err("must be rejected");
    (e.wire_code().to_string(), e.client_message())
}

fn cell_text(c: &Cell) -> String {
    match c {
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    run(core, tenant, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
        .iter()
        .map(|row| row.cells.iter().map(cell_text).collect())
        .collect()
}

fn owned(v: &[&[&str]]) -> Vec<Vec<String>> {
    v.iter()
        .map(|r| r.iter().map(|c| c.to_string()).collect())
        .collect()
}

type Case = (
    &'static str,
    &'static str,
    &'static [&'static [&'static str]],
    &'static [&'static [&'static str]],
);

/// `(名前, 問い合わせ, alice の結果, carol の結果)`。PostgreSQL 17.12 の実機で照合した値。carol は
/// alice の private 行（id 4）が見えないため、private 行にしか一致しない外側行は一致なしと同じ結果になる
/// （存在を観測できない。RLS-10）。
const CASES: &[Case] = &[
    (
        "L1",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.tag = d.tag ORDER BY x.body DESC LIMIT 1) s ON true ORDER BY d.id;",
        &[&["1", "b5"], &["2", "NULL"], &["3", "b5"], &["4", "b9"], &["5", "b5"], &["7", "NULL"]],
        &[&["1", "b5"], &["2", "NULL"], &["3", "b5"], &["5", "b5"], &["7", "NULL"]],
    ),
    (
        "L2",
        "SELECT d.id, s.body FROM docs d LEFT OUTER JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang AND x.tag = 't-b' ORDER BY x.body LIMIT 1) s ON true ORDER BY d.id;",
        &[&["1", "b9"], &["2", "b9"], &["3", "NULL"], &["4", "b9"], &["5", "b9"], &["7", "b9"]],
        &[&["1", "NULL"], &["2", "NULL"], &["3", "NULL"], &["5", "NULL"], &["7", "NULL"]],
    ),
    (
        "L3",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1 OFFSET 1) s ON true ORDER BY d.id;",
        &[&["1", "b7"], &["2", "b7"], &["3", "NULL"], &["4", "b7"], &["5", "b7"], &["7", "b7"]],
        &[&["1", "b5"], &["2", "b5"], &["3", "NULL"], &["5", "b5"], &["7", "b5"]],
    ),
    (
        "L3b",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1 OFFSET 5) s ON true ORDER BY d.id;",
        &[&["1", "NULL"], &["2", "NULL"], &["3", "NULL"], &["4", "NULL"], &["5", "NULL"], &["7", "NULL"]],
        &[&["1", "NULL"], &["2", "NULL"], &["3", "NULL"], &["5", "NULL"], &["7", "NULL"]],
    ),
    (
        "O1",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s ON s.body = 'b9' ORDER BY d.id;",
        &[&["1", "b9"], &["2", "b9"], &["3", "NULL"], &["4", "b9"], &["5", "b9"], &["7", "b9"]],
        &[&["1", "NULL"], &["2", "NULL"], &["3", "NULL"], &["5", "NULL"], &["7", "NULL"]],
    ),
    (
        "O1i",
        "SELECT d.id, s.body FROM docs d JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s ON s.body = 'b9' ORDER BY d.id;",
        &[&["1", "b9"], &["2", "b9"], &["4", "b9"], &["5", "b9"], &["7", "b9"]],
        &[],
    ),
    (
        "O2",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s ON d.lang = 'ja' ORDER BY d.id;",
        &[&["1", "b9"], &["2", "b9"], &["3", "NULL"], &["4", "b9"], &["5", "b9"], &["7", "b9"]],
        &[&["1", "b7"], &["2", "b7"], &["3", "NULL"], &["5", "b7"], &["7", "b7"]],
    ),
    (
        "O2i",
        "SELECT d.id, s.body FROM docs d JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s ON d.lang = 'ja' ORDER BY d.id;",
        &[&["1", "b9"], &["2", "b9"], &["4", "b9"], &["5", "b9"], &["7", "b9"]],
        &[&["1", "b7"], &["2", "b7"], &["5", "b7"], &["7", "b7"]],
    ),
    (
        "O3",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s ON s.body = d.body ORDER BY d.id;",
        &[&["1", "NULL"], &["2", "NULL"], &["3", "b3"], &["4", "b9"], &["5", "NULL"], &["7", "NULL"]],
        &[&["1", "NULL"], &["2", "NULL"], &["3", "b3"], &["5", "NULL"], &["7", "b7"]],
    ),
    (
        "O3i",
        "SELECT d.id, s.body FROM docs d JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s ON s.body = d.body ORDER BY d.id;",
        &[&["3", "b3"], &["4", "b9"]],
        &[&["3", "b3"], &["7", "b7"]],
    ),
    (
        "O4",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body LIMIT 2) s ON s.body > d.body ORDER BY d.id, s.body;",
        &[&["1", "b2"], &["2", "NULL"], &["3", "NULL"], &["4", "NULL"], &["5", "NULL"], &["7", "NULL"]],
        &[&["1", "b2"], &["2", "NULL"], &["3", "NULL"], &["5", "NULL"], &["7", "NULL"]],
    ),
    (
        "W1",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.tag = d.tag ORDER BY x.body DESC LIMIT 1) s ON true WHERE s.body = 'b5' ORDER BY d.id;",
        &[&["1", "b5"], &["3", "b5"], &["5", "b5"]],
        &[&["1", "b5"], &["3", "b5"], &["5", "b5"]],
    ),
    (
        "W2",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.tag = d.tag ORDER BY x.body DESC LIMIT 1) s ON true WHERE d.body >= 'b2' ORDER BY d.id;",
        &[&["2", "NULL"], &["3", "b5"], &["4", "b9"], &["5", "b5"], &["7", "NULL"]],
        &[&["2", "NULL"], &["3", "b5"], &["5", "b5"], &["7", "NULL"]],
    ),
    (
        "C1",
        "SELECT d.id, s.body, s2.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.tag = d.tag ORDER BY x.body DESC LIMIT 1) s ON true JOIN LATERAL (SELECT body FROM docs y WHERE y.body = s.body LIMIT 1) s2 ON true ORDER BY d.id;",
        &[&["1", "b5", "b5"], &["3", "b5", "b5"], &["4", "b9", "b9"], &["5", "b5", "b5"]],
        &[&["1", "b5", "b5"], &["3", "b5", "b5"], &["5", "b5", "b5"]],
    ),
    (
        "C2",
        "SELECT d.id, s.body, s2.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.tag = d.tag ORDER BY x.body DESC LIMIT 1) s ON true LEFT JOIN LATERAL (SELECT body FROM docs y WHERE y.body = s.body LIMIT 1) s2 ON true ORDER BY d.id;",
        &[&["1", "b5", "b5"], &["2", "NULL", "NULL"], &["3", "b5", "b5"], &["4", "b9", "b9"], &["5", "b5", "b5"], &["7", "NULL", "NULL"]],
        &[&["1", "b5", "b5"], &["2", "NULL", "NULL"], &["3", "b5", "b5"], &["5", "b5", "b5"], &["7", "NULL", "NULL"]],
    ),
    (
        "C3",
        "SELECT d.id, s.body, e.id FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.tag = d.tag ORDER BY x.body DESC LIMIT 1) s ON true LEFT JOIN docs e ON e.body = s.body ORDER BY d.id;",
        &[&["1", "b5", "5"], &["2", "NULL", "NULL"], &["3", "b5", "5"], &["4", "b9", "4"], &["5", "b5", "5"], &["7", "NULL", "NULL"]],
        &[&["1", "b5", "5"], &["2", "NULL", "NULL"], &["3", "b5", "5"], &["5", "b5", "5"], &["7", "NULL", "NULL"]],
    ),
    (
        "U1",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = 'zz') s ON true ORDER BY d.id;",
        &[&["1", "NULL"], &["2", "NULL"], &["3", "NULL"], &["4", "NULL"], &["5", "NULL"], &["7", "NULL"]],
        &[&["1", "NULL"], &["2", "NULL"], &["3", "NULL"], &["5", "NULL"], &["7", "NULL"]],
    ),
    (
        "U2",
        "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = 'en') s ON s.body = d.body ORDER BY d.id;",
        &[&["1", "NULL"], &["2", "NULL"], &["3", "b3"], &["4", "NULL"], &["5", "NULL"], &["7", "NULL"]],
        &[&["1", "NULL"], &["2", "NULL"], &["3", "b3"], &["5", "NULL"], &["7", "NULL"]],
    ),
    (
        "U3",
        "SELECT d.id, s.body FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = 'en') s ON true ORDER BY d.id;",
        &[&["1", "b3"], &["2", "b3"], &["3", "b3"], &["4", "b3"], &["5", "b3"], &["7", "b3"]],
        &[&["1", "b3"], &["2", "b3"], &["3", "b3"], &["5", "b3"], &["7", "b3"]],
    ),
    (
        "U3b",
        "SELECT d.id, s.body FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = 'en') s ON d.body > 'zz' ORDER BY d.id;",
        &[&["NULL", "b3"]],
        &[&["NULL", "b3"]],
    ),
    (
        "U4",
        "SELECT d.id, s.body FROM docs d FULL JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = 'en') s ON s.body = d.body ORDER BY d.id, s.body;",
        &[&["1", "NULL"], &["2", "NULL"], &["3", "b3"], &["4", "NULL"], &["5", "NULL"], &["7", "NULL"]],
        &[&["1", "NULL"], &["2", "NULL"], &["3", "b3"], &["5", "NULL"], &["7", "NULL"]],
    ),
    (
        "U5",
        "SELECT d.id, s.body FROM docs d FULL OUTER JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = 'en' AND x.body > 'zz') s ON true ORDER BY d.id;",
        &[&["1", "NULL"], &["2", "NULL"], &["3", "NULL"], &["4", "NULL"], &["5", "NULL"], &["7", "NULL"]],
        &[&["1", "NULL"], &["2", "NULL"], &["3", "NULL"], &["5", "NULL"], &["7", "NULL"]],
    ),
    (
        "U6",
        "SELECT d.id, s.body FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = 'en') s ON s.body = d.body ORDER BY d.id;",
        &[&["3", "b3"]],
        &[&["3", "b3"]],
    ),
    (
        "U7",
        "SELECT d.id, d.lang, d.body FROM docs d NATURAL JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = 'en') s ORDER BY d.id;",
        &[&["3", "en", "b3"]],
        &[&["3", "en", "b3"]],
    ),
];

#[test]
fn left_lateral_forms_match_postgresql_for_every_visible_row_set() {
    let (core, _g) = open();
    for (name, sql, alice, carol) in CASES {
        let bare = sql.trim_end_matches(';');
        // 末尾 `;`（psql の単独送信）の有無で結果が変わらない。
        for s in [bare.to_string(), format!("{bare};")] {
            assert_eq!(rows(&core, "alice", &s), owned(alice), "{name} alice: {s}");
            assert_eq!(rows(&core, "carol", &s), owned(carol), "{name} carol: {s}");
        }
    }
}

#[test]
fn right_and_full_lateral_referencing_the_left_side_is_42p10() {
    let (core, _g) = open();
    let msg = "invalid reference to FROM-clause entry: the combining JOIN type must be INNER or LEFT for a LATERAL reference";
    for sql in [
        "SELECT d.id FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang) s ON true",
        "SELECT d.id FROM docs d FULL JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang) s ON true",
        "SELECT d.id FROM docs d FULL OUTER JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang) s ON true",
        "SELECT d.id FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.nope) s ON true",
        "SELECT d.id FROM docs d FULL JOIN LATERAL (SELECT body FROM docs x WHERE body = d.body) s ON true",
        "SELECT d.id FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang) s ON s.body = d.body",
        "SELECT d.id FROM docs d RIGHT JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang) s USING (lang)",
        "SELECT nope FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang) s ON true",
    ] {
        let a = err_pair(&core, "alice", sql);
        let c = err_pair(&core, "carol", sql);
        assert_eq!(a.0, "42P10", "{sql}: {a:?}");
        assert_eq!(a.1, msg, "{sql}");
        assert_eq!(a, c, "{sql}");
    }
    // 内側の未知列・未知の修飾子は本文のテキスト順で先に現れた方（PostgreSQL と同じ）。
    let (c, _) = err_pair(
        &core,
        "alice",
        "SELECT d.id FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.nope = d.lang) s ON true",
    );
    assert_eq!(c, "42703");
    let (c, _) = err_pair(
        &core,
        "alice",
        "SELECT d.id FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = z.lang) s ON true",
    );
    assert_eq!(c, "42P01");
}

/// PostgreSQL が受理する相関 LATERAL の USING／NATURAL と行ごとの集計は、本実装では固定文言の `0A000`
/// （受け皿の Issue は未起票。`sql62_lateral_exec.rs` の集計の項と同じ扱い）。
#[test]
fn unsupported_correlated_shapes_stay_fail_closed() {
    let (core, _g) = open();
    for sql in [
        "SELECT * FROM docs d JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang ORDER BY x.lang LIMIT 1) s USING (lang)",
        "SELECT * FROM docs d NATURAL LEFT JOIN LATERAL (SELECT lang FROM docs x WHERE x.lang = d.lang ORDER BY x.lang LIMIT 1) s",
        "SELECT * FROM docs d LEFT JOIN LATERAL (SELECT count(*) FROM docs x WHERE x.lang = d.lang) s ON true",
    ] {
        let a = err_pair(&core, "alice", sql);
        assert_eq!(a.0, "0A000", "{sql}: {a:?}");
        assert_eq!(a, err_pair(&core, "carol", sql), "{sql}");
    }
    assert_eq!(rows(&core, "alice", "SELECT * FROM docs").len(), 6);
}

const LEFT_TOP1: &str = "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s ON true ORDER BY d.id";

#[test]
fn null_supplemented_rows_count_toward_the_output_row_cap() {
    // 内側が全外側行で空になる形。結果は NULL 補完の 6 行で、ちょうどは通り 5 行は超過する。
    let sql = "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.tag = d.tag ORDER BY x.body DESC LIMIT 1 OFFSET 7) s ON true ORDER BY d.id";
    let (core, _g) = open_with(|c| c.with_join_limits(JoinSizeLimits::new(REL, 1_000, 6)));
    assert_eq!(rows(&core, "alice", sql).len(), 6);
    let (core, _g) = open_with(|c| c.with_join_limits(JoinSizeLimits::new(REL, 1_000, 5)));
    assert_eq!(err_pair(&core, "alice", sql).0, "54000");
    // 副作用なし。
    assert_eq!(rows(&core, "alice", "SELECT * FROM docs").len(), 6);
}

#[test]
fn reevaluation_cap_counts_outer_tuples_that_pass_the_outer_only_on() {
    // `ON d.lang = 'ja'`（外側だけの ON）を通る外側タプルは 5 件（en の 1 件は再評価されない）。
    let sql = "SELECT d.id, s.body FROM docs d LEFT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s ON d.lang = 'ja' ORDER BY d.id";
    let (core, _g) = open_with(|c| c.with_correlated_limits(CorrelatedLimits::new(5)));
    assert_eq!(rows(&core, "alice", sql).len(), 6);
    let (core, _g) = open_with(|c| c.with_correlated_limits(CorrelatedLimits::new(4)));
    assert_eq!(err_pair(&core, "alice", sql).0, "54000");
    // 全外側タプルを再評価する形は 6 件。
    let (core, _g) = open_with(|c| c.with_correlated_limits(CorrelatedLimits::new(5)));
    assert_eq!(err_pair(&core, "alice", LEFT_TOP1).0, "54000");
}

#[test]
fn describe_matches_the_executed_columns_for_left_and_right() {
    let (core, _g) = open();
    for sql in [
        LEFT_TOP1,
        "SELECT d.id, s.body FROM docs d RIGHT JOIN LATERAL (SELECT body FROM docs x WHERE x.lang = 'en') s ON true ORDER BY d.id",
    ] {
        let session = SessionState::default();
        let prepared = core.parse_sql_prepared(sql).expect("parse");
        let described = core
            .describe_prepared_in_session(&session, &prepared)
            .expect("describe")
            .expect("columns");
        let executed = run(&core, "alice", sql).expect("execute");
        assert_eq!(described, executed.columns, "{sql}");
    }
}
