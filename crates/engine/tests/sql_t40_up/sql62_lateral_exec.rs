//! 左側の列を参照する LATERAL 派生表の外側行ごとの再評価の結合テスト（SQL-62・Issue #3322、親 #3319）。
//! ポインタ: SQL-60・SQL-61・TASK-344・RLS-10。設計記録 `docs/design/lateral-derived-table.md`
//! （L1・L2・L5・L7〜L15）。
//!
//! 期待値は本テストが固定値で持ち、内側に `ORDER BY` を付けて PostgreSQL でも結果が一意に決まる形だけを
//! 使う（素の `LIMIT 1` は PostgreSQL でも非決定的）。alice の可視行と同じ 6 行の表を PostgreSQL 17.12 の
//! 実機へ作り、top-1・OFFSET・NULL の外側値・OR・2 段の LATERAL・外側 WHERE の結果が本テストの alice の
//! 期待値と一致することを確認済み（他テナントの期待値は private 行を除いた同じ規則からの導出）。受理しない形は固定文言の `0A000` のままにし、
//! 各テストの `// tracked:` で受け皿の Issue を示す（受け皿の無い項目は注記を付けず本文に理由を書く）。

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
/// リレーション数の上限（既定値のまま使う）。
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

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn err_of(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    run(core, tenant, sql).expect_err("must be rejected")
}

fn err_pair(core: &EngineCore, tenant: &str, sql: &str) -> (String, String) {
    let e = err_of(core, tenant, sql);
    (e.wire_code().to_string(), e.client_message())
}

/// 公開行は全テナントから見える。alice だけが private 行（id 4。`ja` の最大の body）も見える。
///
/// | id | 所有 | lang | body | tag | 公開範囲 |
/// | -- | ---- | ---- | ---- | --- | -------- |
/// | 1 | alice | ja | b1 | t-a | public |
/// | 2 | alice | ja | b2 | NULL | public |
/// | 3 | alice | en | b3 | t-a | public |
/// | 4 | alice | ja | b9 | t-b | private |
/// | 5 | bob | ja | b5 | t-a | public |
/// | 7 | carol | ja | b7 | NULL | public |
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
    let path = unique_db_path("sql62-lateral-exec");
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

fn cell_text(c: &Cell) -> String {
    match c {
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

fn grid(r: &QueryResult) -> Vec<Vec<String>> {
    r.rows
        .iter()
        .map(|row| row.cells.iter().map(cell_text).collect())
        .collect()
}

fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    grid(&run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}")))
}

fn pairs(v: &[(&str, &str)]) -> Vec<Vec<String>> {
    v.iter()
        .map(|(a, b)| vec![a.to_string(), b.to_string()])
        .collect()
}

const TOP1: &str = "(SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s";

#[test]
fn top_n_per_outer_row_matches_the_expected_result_in_every_join_form() {
    let (core, _g) = open();
    let alice = pairs(&[
        ("1", "b9"),
        ("2", "b9"),
        ("3", "b3"),
        ("4", "b9"),
        ("5", "b9"),
        ("7", "b9"),
    ]);
    for from in [
        format!("docs d, LATERAL {TOP1}"),
        format!("docs d CROSS JOIN LATERAL {TOP1}"),
        format!("docs d JOIN LATERAL {TOP1} ON true"),
        format!("docs d INNER JOIN LATERAL {TOP1} ON true"),
    ] {
        let sql = format!("SELECT d.id, s.body FROM {from} ORDER BY d.id");
        assert_eq!(rows(&core, "alice", &sql), alice, "{sql}");
        // 末尾 `;`（psql の単独送信）でも同じ結果になる。
        assert_eq!(rows(&core, "alice", &format!("{sql};")), alice, "{sql};");
    }
    // 他テナントは private 行が見えず、自分の可視行だけから作った結果になる。
    let sql = format!("SELECT d.id, s.body FROM docs d, LATERAL {TOP1} ORDER BY d.id");
    let others = pairs(&[
        ("1", "b7"),
        ("2", "b7"),
        ("3", "b3"),
        ("5", "b7"),
        ("7", "b7"),
    ]);
    assert_eq!(rows(&core, "bob", &sql), others);
    assert_eq!(rows(&core, "carol", &sql), others);
}

#[test]
fn inner_limit_offset_and_order_apply_per_outer_row() {
    let (core, _g) = open();
    // 外側の行ごとに ja の 2 番目に大きい body（OFFSET 1 LIMIT 1）。en は 1 行しかないため出ない。
    let sql = "SELECT d.id, s.body FROM docs d, LATERAL (SELECT body FROM docs x \
               WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1 OFFSET 1) s ORDER BY d.id";
    assert_eq!(
        rows(&core, "alice", sql),
        pairs(&[
            ("1", "b7"),
            ("2", "b7"),
            ("4", "b7"),
            ("5", "b7"),
            ("7", "b7")
        ])
    );
    // LIMIT 2 で外側 1 行あたり内側の行が 2 件出る（ja の昇順で先頭 2 件。他テナントは private 行が無い）。
    let sql = "SELECT d.id, s.body FROM docs d, LATERAL (SELECT body FROM docs x \
               WHERE x.lang = d.lang ORDER BY x.body LIMIT 2) s WHERE d.lang = 'en' \
               ORDER BY d.id, s.body";
    assert_eq!(rows(&core, "alice", sql), pairs(&[("3", "b3")]));
    let sql = "SELECT d.id, s.body FROM docs d, LATERAL (SELECT body FROM docs x \
               WHERE x.lang = d.lang ORDER BY x.body LIMIT 2) s WHERE d.lang = 'ja' \
               ORDER BY d.id, s.body";
    assert_eq!(
        rows(&core, "bob", sql),
        pairs(&[
            ("1", "b1"),
            ("1", "b2"),
            ("2", "b1"),
            ("2", "b2"),
            ("5", "b1"),
            ("5", "b2"),
            ("7", "b1"),
            ("7", "b2")
        ])
    );
}

#[test]
fn outer_reference_forms_and_column_list_work() {
    let (core, _g) = open();
    // 列名リスト・内側の WHERE に非相関の連言を併用する（`tag = 't-a'` の ja は b1・b5、en は b3）。
    let sql = "SELECT d.id, s.c FROM docs d, LATERAL (SELECT body FROM docs x \
               WHERE x.lang = d.lang AND x.tag = 't-a' ORDER BY x.body DESC LIMIT 1) s(c) \
               ORDER BY d.id";
    assert_eq!(
        rows(&core, "alice", sql),
        pairs(&[
            ("1", "b5"),
            ("2", "b5"),
            ("3", "b3"),
            ("4", "b5"),
            ("5", "b5"),
            ("7", "b5")
        ])
    );
    // OR・括弧の中の外側参照（ja の行 ∪ body = 'b3' の昇順の先頭）。
    let sql = "SELECT d.id, s.body FROM docs d, LATERAL (SELECT body FROM docs x \
               WHERE (x.lang = d.lang OR x.body = 'b3') ORDER BY x.body LIMIT 1) s \
               ORDER BY d.id";
    assert_eq!(
        rows(&core, "alice", sql),
        pairs(&[
            ("1", "b1"),
            ("2", "b1"),
            ("3", "b3"),
            ("4", "b1"),
            ("5", "b1"),
            ("7", "b1")
        ])
    );
    // 左側が CTE で、外側の列を完全に非修飾で参照する（`only_c` は内側の `docs` に無い）。
    let sql = "WITH c(only_c) AS (SELECT lang FROM docs) \
               SELECT c.only_c, s.body FROM c, LATERAL (SELECT body FROM docs \
               WHERE lang = only_c ORDER BY body DESC LIMIT 1) s ORDER BY c.only_c, s.body";
    assert_eq!(
        rows(&core, "bob", sql),
        pairs(&[
            ("en", "b3"),
            ("ja", "b7"),
            ("ja", "b7"),
            ("ja", "b7"),
            ("ja", "b7")
        ])
    );
}

#[test]
fn two_left_relations_and_chained_laterals_and_following_join() {
    let (core, _g) = open();
    // 左側が 2 relation（b の列を参照。a は en の 1 行だけ）。
    let sql = "SELECT b.id, s.body FROM docs a, docs b, LATERAL (SELECT body FROM docs x \
               WHERE x.lang = b.lang ORDER BY x.body DESC LIMIT 1) s \
               WHERE a.lang = 'en' ORDER BY b.id";
    assert_eq!(
        rows(&core, "alice", sql),
        pairs(&[
            ("1", "b9"),
            ("2", "b9"),
            ("3", "b3"),
            ("4", "b9"),
            ("5", "b9"),
            ("7", "b9")
        ])
    );
    // 先行する LATERAL の列を参照する LATERAL（2 段。ja の最大の次に大きい body）。
    let sql = "SELECT d.id, s1.body, s2.body FROM docs d, \
               LATERAL (SELECT body, lang FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s1, \
               LATERAL (SELECT body FROM docs y WHERE y.lang = s1.lang AND y.body < s1.body ORDER BY y.body DESC LIMIT 1) s2 \
               WHERE d.lang = 'ja' ORDER BY d.id";
    assert_eq!(
        rows(&core, "alice", sql),
        [
            ("1", "b9", "b7"),
            ("2", "b9", "b7"),
            ("4", "b9", "b7"),
            ("5", "b9", "b7"),
            ("7", "b9", "b7")
        ]
        .iter()
        .map(|(a, b, c)| vec![a.to_string(), b.to_string(), c.to_string()])
        .collect::<Vec<_>>()
    );
    // LATERAL の後ろに続く JOIN ... ON（s.body = e.body の行 id を引く）。
    let sql = "SELECT d.id, e.id FROM docs d CROSS JOIN LATERAL \
               (SELECT body FROM docs x WHERE x.lang = d.lang ORDER BY x.body DESC LIMIT 1) s \
               JOIN docs e ON e.body = s.body ORDER BY d.id";
    assert_eq!(
        rows(&core, "alice", sql),
        pairs(&[
            ("1", "4"),
            ("2", "4"),
            ("3", "3"),
            ("4", "4"),
            ("5", "4"),
            ("7", "4")
        ])
    );
}

#[test]
fn outer_where_on_lateral_columns_applies_after_the_inner_limit() {
    let (core, _g) = open();
    // 内側 LIMIT の後に外側 WHERE が効く。alice の ja の最大は b9 なので `b7` の行は出ない（内側へ押し込むと
    // b7 が選ばれて出てしまう）。他テナントは ja の最大が b7 なので ja の行が出る。
    let sql = format!(
        "SELECT d.id, s.body FROM docs d, LATERAL {TOP1} WHERE s.body = 'b7' ORDER BY d.id"
    );
    assert_eq!(rows(&core, "alice", &sql), Vec::<Vec<String>>::new());
    assert_eq!(
        rows(&core, "bob", &sql),
        pairs(&[("1", "b7"), ("2", "b7"), ("5", "b7"), ("7", "b7")])
    );
    // 外側 WHERE の列どうしの等価（結合キーへの昇格の対象になる形）も LATERAL の後に効く。
    let sql = format!(
        "SELECT d.id, s.body FROM docs d, LATERAL {TOP1} WHERE s.body = d.body ORDER BY d.id"
    );
    assert_eq!(
        rows(&core, "alice", &sql),
        pairs(&[("3", "b3"), ("4", "b9")])
    );
}

#[test]
fn rows_without_an_inner_match_and_null_outer_values_produce_no_output() {
    let (core, _g) = open();
    // `tag` が NULL の外側行（id 2・7）は一致しない（`x.tag = d.tag` は NULL）。
    let sql = "SELECT d.id, s.body FROM docs d, LATERAL (SELECT body FROM docs x \
               WHERE x.tag = d.tag ORDER BY x.body DESC LIMIT 1) s ORDER BY d.id";
    assert_eq!(
        rows(&core, "alice", sql),
        pairs(&[("1", "b5"), ("3", "b5"), ("4", "b9"), ("5", "b5")])
    );
    // 内側が 0 行になる外側行は出ない（`tag = 't-b'` の行は alice の private 行だけ）。
    let sql = "SELECT d.id, s.body FROM docs d, LATERAL (SELECT body FROM docs x \
               WHERE x.lang = d.lang AND x.tag = 't-b' ORDER BY x.body LIMIT 1) s ORDER BY d.id";
    assert_eq!(
        rows(&core, "alice", sql),
        pairs(&[
            ("1", "b9"),
            ("2", "b9"),
            ("4", "b9"),
            ("5", "b9"),
            ("7", "b9")
        ])
    );
    assert_eq!(rows(&core, "bob", sql), Vec::<Vec<String>>::new());
}

#[test]
fn row_id_columns_compare_without_internal_errors() {
    let (core, _g) = open();
    let all = pairs(&[
        ("1", "b1"),
        ("2", "b2"),
        ("3", "b3"),
        ("4", "b9"),
        ("5", "b5"),
        ("7", "b7"),
    ]);
    let sql = "SELECT d.id, s.body FROM docs d, LATERAL (SELECT body FROM docs x \
               WHERE x.id = d.id ORDER BY x.body LIMIT 1) s ORDER BY d.id";
    assert_eq!(rows(&core, "alice", sql), all);
    // 内側の `id` を非修飾で参照する形。
    let sql = "SELECT d.id, s.body FROM docs d, LATERAL (SELECT body FROM docs x \
               WHERE id = d.id ORDER BY x.body LIMIT 1) s ORDER BY d.id";
    assert_eq!(rows(&core, "alice", sql), all);
}

#[test]
fn tenants_see_only_their_visible_rows_and_errors_are_identical() {
    let (core, _g) = open();
    let sql = format!("SELECT d.id, s.body FROM docs d, LATERAL {TOP1} ORDER BY d.id");
    // alice の private 行（b9）は alice にだけ見え、bob・carol の結果に現れない。
    for tenant in ["bob", "carol"] {
        assert!(
            rows(&core, tenant, &sql).iter().all(|r| r[1] != "b9"),
            "{tenant}"
        );
    }
    assert!(rows(&core, "alice", &sql).iter().any(|r| r[1] == "b9"));
    // 束縛エラー・拒否の文言はテナントに依存せず、予約内部名を含まない。
    for bad in [
        "SELECT * FROM docs d, LATERAL (SELECT body FROM docs x WHERE x.lang = d.nope) s",
        "SELECT * FROM docs d, LATERAL (SELECT body FROM docs x WHERE x.id = d.lang) s",
        "SELECT * FROM docs d, LATERAL (SELECT body FROM docs x WHERE x.lang = d.embedding) s",
    ] {
        let a = err_pair(&core, "alice", bad);
        let c = err_pair(&core, "carol", bad);
        assert_eq!(a, c, "{bad}");
        assert!(
            !a.1.contains('\u{2}') && !a.1.contains("lateral0"),
            "{bad}: {}",
            a.1
        );
    }
}

#[test]
fn reevaluation_cap_rejects_with_54000_and_has_no_side_effects() {
    // alice の外側タプルは 6 件。上限ちょうどなら成功し、1 件でも超えれば `54000`。
    let sql = format!("SELECT d.id, s.body FROM docs d, LATERAL {TOP1} ORDER BY d.id");
    let (core, _g) = open_with(|c| c.with_correlated_limits(CorrelatedLimits::new(6)));
    assert_eq!(rows(&core, "alice", &sql).len(), 6);
    let (core, _g) = open_with(|c| c.with_correlated_limits(CorrelatedLimits::new(5)));
    assert_eq!(err_of(&core, "alice", &sql).wire_code(), "54000");
    // 他テナントは可視行が 5 件のため同じ上限でも通る（上限の判定は可視行だけ）。
    assert_eq!(rows(&core, "carol", &sql).len(), 5);
    // 副作用なし: 拒否の前後で件数が変わらない。
    assert_eq!(rows(&core, "alice", "SELECT * FROM docs").len(), 6);
}

#[test]
fn join_size_caps_reject_with_54000() {
    let sql = format!("SELECT d.id, s.body FROM docs d, LATERAL {TOP1} ORDER BY d.id");
    // 出力行の上限（alice の結果は 6 行。ちょうどは通り、5 行は超過）。
    let (core, _g) = open_with(|c| c.with_join_limits(JoinSizeLimits::new(REL, 1_000, 6)));
    assert_eq!(rows(&core, "alice", &sql).len(), 6);
    let (core, _g) = open_with(|c| c.with_join_limits(JoinSizeLimits::new(REL, 1_000, 5)));
    assert_eq!(err_of(&core, "alice", &sql).wire_code(), "54000");
    // 入力行の上限（外側の `docs d` も基底も alice で 6 行）。
    let (core, _g) = open_with(|c| c.with_join_limits(JoinSizeLimits::new(REL, 6, 1_000)));
    assert_eq!(rows(&core, "alice", &sql).len(), 6);
    let (core, _g) = open_with(|c| c.with_join_limits(JoinSizeLimits::new(REL, 5, 1_000)));
    assert_eq!(err_of(&core, "alice", &sql).wire_code(), "54000");
}

// tracked: #3323
#[test]
fn left_and_non_true_on_forms_stay_fail_closed() {
    let (core, _g) = open();
    for (sql, msg) in [
        (
            format!("SELECT * FROM docs d LEFT JOIN LATERAL {TOP1} ON true"),
            "LEFT JOIN LATERAL is not supported yet",
        ),
        (
            format!("SELECT * FROM docs d LEFT OUTER JOIN LATERAL {TOP1} ON true"),
            "LEFT JOIN LATERAL is not supported yet",
        ),
        (
            format!("SELECT * FROM docs d JOIN LATERAL {TOP1} ON d.id = d.id"),
            "JOIN LATERAL referencing outer columns with an ON condition other than true is not supported yet",
        ),
    ] {
        let (c, m) = err_pair(&core, "alice", &sql);
        assert_eq!(c, "0A000", "{sql}");
        assert!(m.ends_with(msg), "{sql}: {m}");
    }
}

// 行ごとの集計は後続（設計記録 L5。受け皿 Issue は未起票のため追跡注記は付けない）。
#[test]
fn aggregate_distinct_and_star_bodies_stay_fail_closed() {
    let (core, _g) = open();
    for sql in [
        "SELECT * FROM docs d, LATERAL (SELECT count(*) FROM docs x WHERE x.lang = d.lang) s",
        "SELECT * FROM docs d, LATERAL (SELECT DISTINCT lang FROM docs x WHERE x.lang = d.lang) s",
        "SELECT * FROM docs d, LATERAL (SELECT * FROM docs x WHERE x.lang = d.lang) s",
    ] {
        assert_eq!(err_pair(&core, "alice", sql).0, "0A000", "{sql}");
    }
    // 内側の非相関な `IN (SELECT ...)` は通常の派生表と同じ `42601`（本文の判定表が受理しない）。
    let sql = "SELECT * FROM docs d, LATERAL (SELECT body FROM docs x WHERE x.lang = d.lang AND x.body IN (SELECT body FROM docs y)) s";
    assert_eq!(err_pair(&core, "alice", sql).0, "42601", "{sql}");
    // 外側の WHERE のサブクエリと LATERAL の併用は JOIN の WHERE が受理しない形で、相関サブクエリの
    // 再評価予算と二重に数える経路は作られない（SQLSTATE は実測で固定）。
    let sql = format!(
        "SELECT d.id FROM docs d, LATERAL {TOP1} WHERE EXISTS (SELECT 1 FROM docs z WHERE z.lang = d.lang)"
    );
    let (c, m) = err_pair(&core, "alice", &sql);
    assert_eq!(c, "42601", "{m}");
    // 副作用なし。
    assert_eq!(rows(&core, "alice", "SELECT * FROM docs").len(), 6);
}

/// Describe（拡張クエリプロトコル）は基底の評価なしに Execute と同じ結果列を導出する（Describe と Execute は
/// 同じ束縛を通す。設計記録 L17）。
#[test]
fn describe_matches_the_executed_columns() {
    let (core, _g) = open();
    let sql = format!("SELECT d.id, s.body FROM docs d, LATERAL {TOP1} ORDER BY d.id");
    let session = SessionState::default();
    let prepared = core.parse_sql_prepared(&sql).expect("parse");
    let described = core
        .describe_prepared_in_session(&session, &prepared)
        .expect("describe")
        .expect("columns");
    let executed = run(&core, "alice", &sql).expect("execute");
    assert_eq!(described, executed.columns);
}
