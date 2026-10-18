//! `INTERVAL` 列（`ColumnType::Interval`。TABLE-34・Issue #1580）の比較述語・ORDER BY・集計・
//! 二次索引（`OrderedColumnIndex::Interval`）の結合テスト。
//!
//! 比較は三要素そのものではなく正規化キー（1 か月 = 30 日、1 日 = 24 時間）で行うこと
//! （`'1 month'` と `'30 days'` が等価）、索引経路・全走査・Rust 側オラクル（本ファイルで独立に
//! 実装した換算式）の 3 者が完全一致すること、MIN/MAX が元の三要素を返すこと、SUM/AVG の
//! PostgreSQL 17 準拠の固定ベクタ、他テナント行の非漏えい（RLS）、未解禁機能（GROUP BY・
//! DISTINCT・window・集合演算・式・PK/UNIQUE 等）が fail-closed のままであることを固定する。
//! 式・算術は #1581、EXTRACT は #1582、OID 1186・バイナリ・NoSQL は #1583 の担当。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::datetime::{format_interval, parse_interval, Interval};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const ROWS: u64 = 64;
const OTHER_TENANT_ID: u64 = 900;

/// 値プール（リテラル）。正規化キーが同じ値（`1 month`・`30 days`・`720 hours`、`0` と
/// `1 mon -30 days`、`1 day` と `24:00:00`、`1 year` と `360 days`）を意図的に含める。
const POOL: [&str; 12] = [
    "1 month",
    "30 days",
    "720 hours",
    "1 mon -30 days",
    "0",
    "1 day",
    "24:00:00",
    "-1 day",
    "2 mons",
    "1 year",
    "359 days 23:59:59.999999",
    "25 hours",
];

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("v", ColumnType::Interval, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 実装とは独立に書いた換算式（PG の `interval_cmp_value` 相当）。
fn norm(v: &Interval) -> i128 {
    (i128::from(v.months) * 30 + i128::from(v.days)) * 86_400_000_000 + i128::from(v.micros)
}

fn iv(s: &str) -> Interval {
    parse_interval(s).unwrap_or_else(|e| panic!("{s}: {e:?}"))
}

/// `id % 9 == 0` は NULL、それ以外はプールから決定的に選ぶ。
fn value_of(id: u64) -> Option<Interval> {
    if id.is_multiple_of(9) {
        None
    } else {
        Some(iv(POOL[(id % 12) as usize]))
    }
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, v: Option<Interval>, vis: Visibility) {
    let t = v.map(Value::Interval).unwrap_or(Value::Null);
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[Value::Vector(vec![id as f32, 0.0]), t],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let (core, guard, _p) = open_core_with_path(label);
    (core, guard)
}

fn open_core_with_path(label: &str) -> (EngineCore, CleanupGuard, std::path::PathBuf) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for id in 1..=ROWS {
        insert_row(&storage, "tenant-a", id, value_of(id), Visibility::Public);
    }
    // 他テナントの Private 行（どの述語にも合う値）。tenant-a からは見えてはならない。
    insert_row(
        &storage,
        "tenant-b",
        OTHER_TENANT_ID,
        Some(iv("30 days")),
        Visibility::Private,
    );
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
        path,
    )
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

fn try_run(core: &EngineCore, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    core.execute_sql(&ctx("tenant-a"), sql)
}

fn code(core: &EngineCore, sql: &str) -> String {
    try_run(core, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

fn ids(result: &QueryResult) -> Vec<u64> {
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn oracle(pred: impl Fn(i128) -> bool) -> Vec<u64> {
    (1..=ROWS)
        .filter(|id| value_of(*id).is_some_and(|v| pred(norm(&v))))
        .collect()
}

fn cell_str(c: &Cell) -> String {
    match c {
        Cell::Interval(v) => format_interval(v),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

fn first_col(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| r.cells.first().map(cell_str).unwrap_or_default())
        .collect()
}

fn scalar(core: &EngineCore, sql: &str) -> String {
    first_col(&run(core, sql)).remove(0)
}

/// 述語を索引経路（DISTANCE）・全走査強制（`id + 0 > 0`）・単純な走査で実行し、
/// オラクルと完全一致することを固定する。選択度が十分低い述語では索引が消費されることも確認する。
fn assert_parity(core: &EngineCore, pred: &str, expected: &[u64], expect_index: bool) {
    let vec_sql =
        format!("SELECT id FROM {TABLE} WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200");
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, &vec_sql);
    let hot = run(core, &vec_sql);
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(ids(&cold), ids(&hot), "cold/hot mismatch: {pred}");
    assert_eq!(ids(&hot), expected, "oracle mismatch: {pred}");
    if expect_index && !expected.is_empty() && expected.len() * 2 <= ROWS as usize {
        assert!(after > before, "index not consumed: {pred}");
    }
    let scan_sql = format!(
        "SELECT id FROM {TABLE} WHERE {pred} AND id + 0 > 0 \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
    );
    assert_eq!(ids(&run(core, &scan_sql)), expected, "full scan: {pred}");
    let plain_sql = format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 200");
    assert_eq!(ids(&run(core, &plain_sql)), expected, "plain: {pred}");
}

fn all_checks(core: &EngineCore) {
    let key = |s: &str| norm(&iv(s));
    for lit in [
        "30 days",
        "1 month",
        "0",
        "24 hours",
        "-1 day",
        "2 mons",
        "360 days",
        "1000 days",
        "-1000 days",
    ] {
        let k = key(lit);
        assert_parity(core, &format!("v = '{lit}'"), &oracle(|x| x == k), true);
        assert_parity(core, &format!("v < '{lit}'"), &oracle(|x| x < k), true);
        assert_parity(core, &format!("v <= '{lit}'"), &oracle(|x| x <= k), true);
        assert_parity(core, &format!("v > '{lit}'"), &oracle(|x| x > k), true);
        assert_parity(core, &format!("v >= '{lit}'"), &oracle(|x| x >= k), true);
    }
    let (lo, hi) = (key("1 day"), key("59 days"));
    assert_parity(
        core,
        "v BETWEEN '1 day' AND '59 days'",
        &oracle(|x| (lo..=hi).contains(&x)),
        true,
    );
    assert_parity(
        core,
        "v NOT BETWEEN '1 day' AND '59 days'",
        &oracle(|x| !(lo..=hi).contains(&x)),
        false,
    );
    assert_parity(
        core,
        "v IN ('30 days', '0')",
        &oracle(|x| x == key("30 days") || x == 0),
        false,
    );
    assert_parity(
        core,
        "v NOT IN ('30 days', '0')",
        &oracle(|x| x != key("30 days") && x != 0),
        false,
    );
    assert_parity(
        core,
        "NOT v = '30 days'",
        &oracle(|x| x != key("30 days")),
        false,
    );
    assert_parity(
        core,
        "v = '30 days' OR v = '2 mons'",
        &oracle(|x| x == key("30 days") || x == key("2 mons")),
        false,
    );
    // `<>` は正規化等価で除外する。
    assert_parity(
        core,
        "v <> '30 days'",
        &oracle(|x| x != key("30 days")),
        false,
    );
}

/// 受け入れ条件: `'1 month'` と `'30 days'` は等価。両方の行が返る。
#[test]
fn one_month_equals_thirty_days() {
    let (core, _g) = open_core("iq-accept");
    let expect = oracle(|x| x == 30 * 86_400_000_000);
    assert!(expect.len() >= 6, "fixture must contain several equal rows");
    for lit in ["30 days", "1 month", "720 hours", "1 mon"] {
        let got = ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE v = '{lit}' LIMIT 200"),
        ));
        assert_eq!(got, expect, "{lit}");
    }
    // 格納値は元の三要素のまま返る（正規化されない）。
    let r = run(
        &core,
        &format!("SELECT id, v FROM {TABLE} WHERE v = '30 days' LIMIT 200"),
    );
    let mut raw = std::collections::BTreeSet::new();
    for row in &r.rows {
        let Some(Cell::Interval(v)) = row.cells.get(1) else {
            panic!("expected interval cell");
        };
        assert_eq!(Some(*v), value_of(row.id));
        raw.insert(format_interval(v));
    }
    assert_eq!(
        raw.len(),
        3,
        "1 mon / 30 days / 720:00:00 stay distinct: {raw:?}"
    );
}

#[test]
fn comparison_predicates_match_index_scan_and_oracle() {
    let (core, _g) = open_core("iq-cmp");
    all_checks(&core);
}

#[test]
fn declared_index_gives_the_same_results_as_the_automatic_one() {
    let (core, _g) = open_core("iq-decl-index");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_v ON docs (v)",
    )
    .expect("CREATE INDEX on INTERVAL must be accepted");
    all_checks(&core);
}

#[test]
fn index_tracks_dml_and_reopen() {
    let (core, _g, path) = open_core_with_path("iq-dml");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_v ON docs (v)",
    )
    .expect("create index");
    assert_parity(
        &core,
        "v = '2 mons'",
        &oracle(|x| x == norm(&iv("60 days"))),
        true,
    );
    // UPDATE 後に索引が無効化され、結果が追随する。
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "UPDATE docs SET v = '60 days' WHERE id = 1 USING OPERATION_ID 'iq-upd-1'",
    )
    .expect("update");
    let mut want = oracle(|x| x == norm(&iv("60 days")));
    want.push(1);
    want.sort_unstable();
    want.dedup();
    let got = ids(&run(
        &core,
        &format!("SELECT id FROM {TABLE} WHERE v = '2 mons' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"),
    ));
    assert_eq!(got, want);
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'iq-del-1'",
    )
    .expect("delete");
    let got = ids(&run(
        &core,
        &format!("SELECT id FROM {TABLE} WHERE v = '2 mons' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"),
    ));
    assert!(!got.contains(&1));
    drop(core);

    // 再起動後も比較結果が変わらない。
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let got = ids(&run(
        &core,
        &format!("SELECT id FROM {TABLE} WHERE v = '60 days' LIMIT 200"),
    ));
    let want: Vec<u64> = oracle(|x| x == norm(&iv("60 days")))
        .into_iter()
        .filter(|id| *id != 1)
        .collect();
    assert_eq!(got, want);
}

#[test]
fn null_predicates_and_null_never_matches() {
    let (core, _g) = open_core("iq-null");
    let nulls: Vec<u64> = (1..=ROWS).filter(|id| value_of(*id).is_none()).collect();
    assert_eq!(
        ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE v IS NULL LIMIT 200")
        )),
        nulls
    );
    // NULL 行は比較・NOT 比較のどちらにも現れない（三値論理）。
    let eq = ids(&run(
        &core,
        &format!("SELECT id FROM {TABLE} WHERE v = '0' LIMIT 200"),
    ));
    let ne = ids(&run(
        &core,
        &format!("SELECT id FROM {TABLE} WHERE NOT v = '0' LIMIT 200"),
    ));
    assert!(eq.iter().all(|i| !nulls.contains(i)));
    assert!(ne.iter().all(|i| !nulls.contains(i)));
    assert_eq!(eq.len() + ne.len() + nulls.len(), ROWS as usize);
}

#[test]
fn invalid_literals_are_classified_without_echoing_values() {
    let (core, _g) = open_core("iq-badlit");
    for (lit, want) in [
        ("'zz'", "22007"),
        ("'2147483648 days'", "22015"),
        ("'178956971 years'", "22008"),
        ("'infinity'", "0A000"),
    ] {
        let sql = format!("SELECT id FROM {TABLE} WHERE v = {lit} LIMIT 5");
        assert_eq!(code(&core, &sql), want, "{sql}");
        let err = try_run(&core, &sql).expect_err("must fail");
        assert!(
            !format!("{err:?}").contains("tenant-b"),
            "error must not carry other-tenant data"
        );
    }
    // 数値・真偽リテラルは型不一致。
    for lit in ["12", "true"] {
        let sql = format!("SELECT id FROM {TABLE} WHERE v = {lit} LIMIT 5");
        assert!(try_run(&core, &sql).is_err(), "{sql}");
    }
}

#[test]
fn prepared_parameters_bind_and_describe() {
    let (core, _g) = open_core("iq-param");
    let mut session = SessionState::default();
    let prepared = core
        .parse_sql_prepared(&format!("SELECT id FROM {TABLE} WHERE v = $1 LIMIT 200"))
        .expect("prepare");
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe must not break for INTERVAL");
    let bound = core
        .bind_prepared(&prepared, &[Some(b"1 month".to_vec())])
        .expect("bind");
    let SqlOutcome::Query(r) = core
        .execute_parsed_in_session(&ctx("tenant-a"), &mut session, &bound)
        .expect("execute")
    else {
        panic!("expected rows");
    };
    assert_eq!(ids(&r), oracle(|x| x == 30 * 86_400_000_000));

    // 不正な値は Bind 時または Execute 時に 22007。
    let bad = core.bind_prepared(
        &core
            .parse_sql_prepared(&format!("SELECT id FROM {TABLE} WHERE v = $1 LIMIT 5"))
            .expect("prepare"),
        &[Some(b"zz".to_vec())],
    );
    let failed = match bad {
        Err(e) => e.wire_code().to_string(),
        Ok(p) => core
            .execute_parsed_in_session(&ctx("tenant-a"), &mut session, &p)
            .expect_err("must fail")
            .wire_code()
            .to_string(),
    };
    assert_eq!(failed, "22007");
}

#[test]
fn order_by_follows_normalized_order_and_postgres_null_defaults() {
    let (core, _g) = open_core("iq-order");
    let mut present: Vec<Interval> = (1..=ROWS).filter_map(value_of).collect();
    present.sort_by_key(norm);
    let nulls = (1..=ROWS).filter(|id| value_of(*id).is_none()).count();

    // キー（正規化値）の昇順で並ぶ。同順位の行の順序は id で確定する。
    let asc = run(
        &core,
        &format!("SELECT id, v FROM {TABLE} ORDER BY v ASC, id ASC LIMIT 200"),
    );
    let got: Vec<(i128, u64)> = asc
        .rows
        .iter()
        .filter_map(|r| match r.cells.get(1) {
            Some(Cell::Interval(v)) => Some((norm(v), r.id)),
            _ => None,
        })
        .collect();
    let mut want: Vec<(i128, u64)> = (1..=ROWS)
        .filter_map(|id| value_of(id).map(|v| (norm(&v), id)))
        .collect();
    want.sort_unstable();
    assert_eq!(got, want, "ASC order by normalized key then id");
    assert!(
        asc.rows
            .iter()
            .rev()
            .take(nulls)
            .all(|r| matches!(r.cells.get(1), Some(Cell::Null))),
        "ASC: NULLS LAST by default"
    );
    assert_eq!(asc.rows.len(), present.len() + nulls);

    let desc = run(
        &core,
        &format!("SELECT id, v FROM {TABLE} ORDER BY v DESC, id ASC LIMIT 200"),
    );
    assert!(
        desc.rows
            .iter()
            .take(nulls)
            .all(|r| matches!(r.cells.get(1), Some(Cell::Null))),
        "DESC: NULLS FIRST by default"
    );
    let got: Vec<i128> = desc
        .rows
        .iter()
        .skip(nulls)
        .filter_map(|r| match r.cells.get(1) {
            Some(Cell::Interval(v)) => Some(norm(v)),
            _ => None,
        })
        .collect();
    let mut want: Vec<i128> = present.iter().map(norm).collect();
    want.reverse();
    assert_eq!(got, want);

    // 上位 k 件（LIMIT 付き）は全件ソートの先頭と一致する。
    let top = run(
        &core,
        &format!("SELECT id FROM {TABLE} ORDER BY v DESC, id ASC LIMIT 5"),
    );
    let mut all: Vec<(i128, u64)> = (1..=ROWS)
        .filter_map(|id| value_of(id).map(|v| (norm(&v), id)))
        .collect();
    all.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    // DESC は NULL が先頭（PG 既定）。
    let nulls: Vec<u64> = (1..=ROWS).filter(|id| value_of(*id).is_none()).collect();
    let mut want: Vec<u64> = nulls.clone();
    want.extend(all.iter().map(|x| x.1));
    want.truncate(5);
    assert_eq!(top.rows.iter().map(|r| r.id).collect::<Vec<_>>(), want);
}

#[test]
fn aggregates_count_min_max_sum_avg() {
    let (core, _g) = open_core("iq-agg");
    let vals: Vec<Interval> = (1..=ROWS).filter_map(value_of).collect();
    let mut distinct: Vec<i128> = vals.iter().map(norm).collect();
    distinct.sort_unstable();
    distinct.dedup();

    let r = run(
        &core,
        &format!("SELECT COUNT(v), COUNT(DISTINCT v) FROM {TABLE}"),
    );
    let row: Vec<String> = r.rows[0].cells.iter().map(cell_str).collect();
    assert_eq!(
        row,
        vec![vals.len().to_string(), distinct.len().to_string()]
    );

    // MIN/MAX は正規化キーで比べ、元の三要素を返す。
    let min = scalar(&core, &format!("SELECT MIN(v) FROM {TABLE}"));
    assert_eq!(min, "-1 days");
    let max = scalar(&core, &format!("SELECT MAX(v) FROM {TABLE}"));
    assert_eq!(norm(&iv(&max)), *distinct.last().expect("non-empty"));

    // SUM は三要素ごとの和（正規化しない）。
    let want = vals.iter().fold(
        Interval {
            months: 0,
            days: 0,
            micros: 0,
        },
        |a, b| Interval {
            months: a.months + b.months,
            days: a.days + b.days,
            micros: a.micros + b.micros,
        },
    );
    assert_eq!(
        scalar(&core, &format!("SELECT SUM(v) FROM {TABLE}")),
        format_interval(&want)
    );
    assert_ne!(
        scalar(&core, &format!("SELECT AVG(v) FROM {TABLE}")),
        "NULL"
    );
}

/// MIN/MAX の同点規則（PG17 の遷移関数と同じ後勝ち）を、走査順が決まる単純な表で固定する。
#[test]
fn min_max_tie_keeps_the_later_observed_value() {
    let path = unique_db_path("iq-tie");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage.create_table(&schema()).expect("table");
    insert_row(
        &storage,
        "tenant-a",
        1,
        Some(iv("1 month")),
        Visibility::Public,
    );
    insert_row(
        &storage,
        "tenant-a",
        2,
        Some(iv("30 days")),
        Visibility::Public,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        scalar(&core, &format!("SELECT MIN(v) FROM {TABLE}")),
        "30 days"
    );
    assert_eq!(
        scalar(&core, &format!("SELECT MAX(v) FROM {TABLE}")),
        "30 days"
    );
}

fn small_table(label: &str, rows: &[Option<&str>]) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage.create_table(&schema()).expect("table");
    for (i, v) in rows.iter().enumerate() {
        insert_row(
            &storage,
            "tenant-a",
            1 + i as u64,
            v.map(iv),
            Visibility::Public,
        );
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

/// PG17 の `interval_pl`・`interval_avg` に準拠した固定ベクタ。
#[test]
fn sum_and_avg_fixed_vectors() {
    let (core, _g) = small_table("iq-vec1", &[Some("1 mon"), Some("30 days")]);
    assert_eq!(
        scalar(&core, &format!("SELECT SUM(v) FROM {TABLE}")),
        "1 mon 30 days"
    );
    let (core, _g) = small_table("iq-vec2", &[Some("1 mon"), Some("2 mons")]);
    assert_eq!(
        scalar(&core, &format!("SELECT AVG(v) FROM {TABLE}")),
        "1 mon 15 days"
    );
    let (core, _g) = small_table("iq-vec3", &[Some("1 day"), Some("2 days")]);
    assert_eq!(
        scalar(&core, &format!("SELECT AVG(v) FROM {TABLE}")),
        "1 day 12:00:00"
    );
    let (core, _g) = small_table("iq-vec4", &[Some("1 mon"), Some("00:00:00")]);
    assert_eq!(
        scalar(&core, &format!("SELECT AVG(v) FROM {TABLE}")),
        "15 days"
    );
    // NULL は無視し、全 NULL・0 行は NULL。
    let (core, _g) = small_table("iq-vec5", &[None, None]);
    assert_eq!(
        scalar(&core, &format!("SELECT SUM(v) FROM {TABLE}")),
        "NULL"
    );
    assert_eq!(
        scalar(&core, &format!("SELECT AVG(v) FROM {TABLE}")),
        "NULL"
    );
    assert_eq!(
        scalar(&core, &format!("SELECT MIN(v) FROM {TABLE}")),
        "NULL"
    );
    assert_eq!(
        scalar(&core, &format!("SELECT COUNT(DISTINCT v) FROM {TABLE}")),
        "0"
    );
    let (core, _g) = small_table("iq-vec6", &[]);
    assert_eq!(
        scalar(&core, &format!("SELECT SUM(v) FROM {TABLE}")),
        "NULL"
    );
    // 和があふれたら 22008（panic しない）。
    let (core, _g) = small_table(
        "iq-vec7",
        &[Some("178000000 years"), Some("178000000 years")],
    );
    assert_eq!(code(&core, &format!("SELECT SUM(v) FROM {TABLE}")), "22008");
    assert_eq!(code(&core, &format!("SELECT AVG(v) FROM {TABLE}")), "22008");
}

#[test]
fn count_distinct_uses_the_normalized_key() {
    let (core, _g) = small_table(
        "iq-cd",
        &[
            Some("1 mon"),
            Some("30 days"),
            Some("720 hours"),
            Some("1 day"),
        ],
    );
    assert_eq!(
        scalar(&core, &format!("SELECT COUNT(DISTINCT v) FROM {TABLE}")),
        "2"
    );
    assert_eq!(scalar(&core, &format!("SELECT COUNT(v) FROM {TABLE}")), "4");
}

#[test]
fn group_by_other_column_with_interval_aggregate() {
    let path = unique_db_path("iq-gb");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("v", ColumnType::Interval, true),
                ColumnDef::new("g", ColumnType::Text, false),
            ],
        ))
        .expect("table");
    for (id, g, v) in [
        (1u64, "a", "1 day"),
        (2, "a", "3 days"),
        (3, "b", "2 days"),
        (4, "b", "4 days"),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx("tenant-a"),
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Interval(iv(v)),
                Value::Text(g.to_string()),
            ],
            &OperationId::parse(&format!("gb-{id}")).expect("op"),
        )
        .expect("insert");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let r = run(
        &core,
        &format!("SELECT g, MIN(v), MAX(v), SUM(v) FROM {TABLE} GROUP BY g ORDER BY g LIMIT 10"),
    );
    let rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|r| r.cells.iter().map(cell_str).collect())
        .collect();
    assert_eq!(
        rows,
        vec![
            vec!["Text(\"a\")", "1 day", "3 days", "4 days"],
            vec!["Text(\"b\")", "2 days", "4 days", "6 days"],
        ]
        .into_iter()
        .map(|r| r.into_iter().map(String::from).collect::<Vec<_>>())
        .collect::<Vec<_>>()
    );
    // 集計結果の並べ替えは正規化キーで行う。
    let r = run(
        &core,
        &format!("SELECT g, MAX(v) AS m FROM {TABLE} GROUP BY g ORDER BY m DESC LIMIT 10"),
    );
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn tenant_private_rows_never_leak() {
    let (core, _g) = open_core("iq-rls");
    for w in [
        "v = '30 days'",
        "v >= '1 month'",
        "v BETWEEN '30 days' AND '30 days'",
        "v IN ('720 hours')",
    ] {
        for sql in [
            format!(
                "SELECT id FROM {TABLE} WHERE {w} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
            ),
            format!("SELECT id FROM {TABLE} WHERE {w} AND id + 0 > 0 LIMIT 200"),
        ] {
            let got = ids(&run(&core, &sql));
            assert!(!got.contains(&OTHER_TENANT_ID), "leak via {sql}");
        }
    }
    let ordered = run(
        &core,
        &format!("SELECT id FROM {TABLE} ORDER BY v DESC LIMIT 200"),
    );
    assert!(!ids(&ordered).contains(&OTHER_TENANT_ID));
    let present = (1..=ROWS).filter(|id| value_of(*id).is_some()).count();
    assert_eq!(
        scalar(&core, &format!("SELECT COUNT(v) FROM {TABLE}")),
        present.to_string()
    );
    // 他テナントの '30 days' は SUM・MIN・MAX にも影響しない。
    let want_max = (1..=ROWS)
        .filter_map(value_of)
        .max_by_key(norm)
        .map(|v| norm(&v))
        .expect("non-empty");
    assert_eq!(
        norm(&iv(&scalar(&core, &format!("SELECT MAX(v) FROM {TABLE}")))),
        want_max
    );
}

/// 未解禁の機能は他型の腕へ黙って流さず fail-closed に拒否され、副作用を残さない。
#[test]
fn features_still_disabled_are_rejected_fail_closed() {
    let (core, _g) = open_core("iq-failclosed");
    for sql in [
        "SELECT v FROM docs GROUP BY v",
        "SELECT v, COUNT(*) FROM docs GROUP BY v",
        "SELECT DISTINCT v FROM docs LIMIT 5",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY v) FROM docs LIMIT 5",
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY v ORDER BY id) FROM docs LIMIT 5",
        "SELECT id, MIN(v) OVER () FROM docs LIMIT 5",
        "SELECT id, COUNT(v) OVER () FROM docs LIMIT 5",
        "SELECT v FROM docs UNION SELECT v FROM docs",
        "SELECT v FROM docs UNION ALL SELECT v FROM docs",
        "SELECT id FROM docs WHERE v IN (SELECT v FROM docs LIMIT 3) ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5",
        "SELECT g, MIN(v) AS lo FROM (SELECT id AS g, v FROM docs) s GROUP BY g HAVING lo > '1 day'",
        "SELECT MIN(v) AS lo FROM docs GROUP BY id HAVING lo > '1 day'",
        "SELECT v + 1 FROM docs LIMIT 5",
        "SELECT id FROM docs WHERE v IS NOT DISTINCT FROM '1 day' LIMIT 5",
        "SELECT INTERVAL '1 day' FROM docs LIMIT 5",
    ] {
        assert!(try_run(&core, sql).is_err(), "{sql} must be rejected");
    }
    // DML・DDL は DDL 許可セッションで実行しても拒否され、副作用を残さない。
    for sql in [
        "CREATE TABLE a (v INTERVAL PRIMARY KEY)",
        "CREATE TABLE a (v INTERVAL UNIQUE)",
        // INTERVAL→TEXT は TABLE-40 の一般変換（Issue #1636）で受理されるためここでは扱わない。
        "ALTER TABLE docs ALTER COLUMN v TYPE INTEGER",
    ] {
        let mut session = SessionState::default();
        session.allow_ddl();
        let e = core
            .execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
            .expect_err(sql);
        assert!(
            ["0A000", "22000", "42601", "42804", "42P16"].contains(&e.wire_code()),
            "{sql}: {e:?}"
        );
    }
    // 拒否された文は何も変えない。
    let present = (1..=ROWS).filter(|id| value_of(*id).is_some()).count();
    assert_eq!(
        scalar(&core, &format!("SELECT COUNT(v) FROM {TABLE}")),
        present.to_string()
    );
    assert_eq!(
        format_interval(&iv(&scalar(
            &core,
            &format!("SELECT v FROM {TABLE} WHERE id = 1 LIMIT 1")
        ))),
        format_interval(&value_of(1).expect("value"))
    );
}
