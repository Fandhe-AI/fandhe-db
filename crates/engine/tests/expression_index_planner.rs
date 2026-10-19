//! 式索引のプランナ選択と EXPLAIN 表示（INDEX-10、Issue #1650）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10・INDEX-7。
//!
//! `tests/index_declaration_numeric.rs` と同じ構成（HNSW opt-in の宣言付きエンジン・
//! `MIN_INDEXED_ROWS` 超の行数・他テナントの `Private` 行）で、WHERE の式述語が宣言済みの式索引と
//! 構造一致したときだけ式キー索引で候補を絞ること、一致しない形・opt-in なしでは従来どおりの
//! 全走査になること、EXPLAIN の `scalar_plan:` 行に INDEX-7 と同じ規則で `index=<索引名>` が出る
//! ことを固定する。索引経路が実際に発火したことは `ScalarIndexCacheStats` で確かめる
//! （一致判定が空振りして常に全走査になる状態を見逃さない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;
use engine::storage::Visibility;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超。公開行の id は 1..=ROWS。
const ROWS: u64 = 1_100;
// テナント境界確認用 Private 行の id（式 `lower(tag) = 'ja'` に一致する値を持たせる）。
const PRIVATE_ID: u64 = 1_111;
const MICROS_PER_DAY: i64 = 86_400_000_000;

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("tag", ColumnType::Text, true),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("ts", ColumnType::Timestamp, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `id` から決まる行の値。`tag` は大文字小文字が混在し 7 の倍数は NULL、`qty` は 11 の倍数が NULL、
/// `ts` は 13 の倍数が NULL。
fn tag_of(id: u64) -> Option<&'static str> {
    if id.is_multiple_of(7) {
        return None;
    }
    Some(["Ja", "JA", "ja", "En", "EN", "Fr", "DE", "Es"][(id % 8) as usize])
}

fn qty_of(id: u64) -> Option<i32> {
    (!id.is_multiple_of(11)).then(|| (id % 12) as i32 - 4)
}

fn ts_of(id: u64) -> Option<i64> {
    (!id.is_multiple_of(13))
        .then(|| (id % 20) as i64 * MICROS_PER_DAY + (id as i64 % 86_400) * 1_000_000)
}

fn insert(storage: &Storage, id: u64, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx("tenant-a"),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 1.0]),
            Value::Text(if id.is_multiple_of(2) { "a" } else { "b" }.to_string()),
            Value::Text(format!("docs/{id}.md")),
            Value::Text(format!("body {id}")),
            tag_of(id)
                .map(|t| Value::Text(t.to_string()))
                .unwrap_or(Value::Null),
            qty_of(id).map(Value::Integer).unwrap_or(Value::Null),
            ts_of(id).map(Value::Timestamp).unwrap_or(Value::Null),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// `USING PLAN` 用の決定的スタブ（検索語・ヒントなし）。
struct StubLlmClient;

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#.to_string())
    }
}

fn seed(label: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for id in 1..=ROWS {
        insert(&storage, id, Visibility::Public);
    }
    // 他テナント視点では見えない Private 行（`lower(tag) = 'ja'`・`qty + 1 = 4` に一致する値）。
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx("tenant-a"),
        PRIVATE_ID,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 1.0]),
            Value::Text("a".to_string()),
            Value::Text("docs/private.md".to_string()),
            Value::Text("body private".to_string()),
            Value::Text("JA".to_string()),
            Value::Integer(3),
            Value::Timestamp(4 * MICROS_PER_DAY),
        ],
        &OperationId::parse("seed-private").expect("valid operation id"),
    )
    .expect("insert private row");
    (storage, guard)
}

/// HNSW opt-in 付き（宣言と式キーが有効）のエンジン。
fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let (storage, guard) = seed(label);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    (
        EngineCore::from_storage_with_engine(storage, kind)
            .with_query_planner(Box::new(StubLlmClient)),
        guard,
    )
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("DDL must succeed: {sql}: {e:?}"));
}

fn ids(core: &EngineCore, tenant: &str, where_clause: &str) -> Vec<u64> {
    let sql = format!(
        "SELECT id FROM docs WHERE {where_clause} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2000"
    );
    let result = core
        .execute_sql(&ctx(tenant), &sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"));
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn explain_sql(core: &EngineCore, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("EXPLAIN must succeed: {sql}: {e:?}"))
    {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(Cell::Text(s)) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

fn explain(core: &EngineCore, where_clause: &str) -> Vec<String> {
    explain_sql(
        core,
        &format!(
            "EXPLAIN SELECT id FROM docs WHERE {where_clause} \
             ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
        ),
    )
}

fn scalar_plan(lines: &[String]) -> String {
    lines
        .iter()
        .find(|l| l.starts_with("scalar_plan: "))
        .cloned()
        .unwrap_or_else(|| panic!("no scalar_plan line: {lines:?}"))
}

/// 公開行を全走査で数えるオラクル（`pred` に行 id を渡す）。
fn oracle(pred: impl Fn(u64) -> bool) -> Vec<u64> {
    (1..=ROWS).filter(|id| pred(*id)).collect()
}

fn lower_tag_is(id: u64, want: &str) -> bool {
    tag_of(id).is_some_and(|t| t.to_lowercase() == want)
}

fn qty_plus_one(id: u64) -> Option<f64> {
    qty_of(id).map(|q| f64::from(q) + 1.0)
}

fn day_of(id: u64) -> Option<i64> {
    ts_of(id).map(|t| t.div_euclid(MICROS_PER_DAY))
}

/// 述語と全走査オラクルの組（宣言した 3 式で索引対応になる形）。
fn cases() -> Vec<(String, Vec<u64>)> {
    vec![
        (
            "lower(tag) = 'ja'".to_string(),
            oracle(|id| lower_tag_is(id, "ja")),
        ),
        (
            "'ja' = lower(tag)".to_string(),
            oracle(|id| lower_tag_is(id, "ja")),
        ),
        (
            "lower(tag) > 'ja'".to_string(),
            oracle(|id| tag_of(id).is_some_and(|t| t.to_lowercase().as_str() > "ja")),
        ),
        (
            "lower(tag) <= 'en'".to_string(),
            oracle(|id| tag_of(id).is_some_and(|t| t.to_lowercase().as_str() <= "en")),
        ),
        (
            "qty + 1 > 5".to_string(),
            oracle(|id| qty_plus_one(id).is_some_and(|v| v > 5.0)),
        ),
        (
            "5 < qty + 1".to_string(),
            oracle(|id| qty_plus_one(id).is_some_and(|v| v > 5.0)),
        ),
        (
            "qty + 1 = 4".to_string(),
            oracle(|id| qty_plus_one(id).is_some_and(|v| v == 4.0)),
        ),
        (
            "qty + 1 >= 2 AND qty + 1 <= 4".to_string(),
            oracle(|id| qty_plus_one(id).is_some_and(|v| (2.0..=4.0).contains(&v))),
        ),
        (
            "date_trunc('day', ts) = TIMESTAMP '1970-01-05 00:00:00'".to_string(),
            oracle(|id| day_of(id) == Some(4)),
        ),
        (
            "date_trunc('day', ts) >= TIMESTAMP '1970-01-15 00:00:00'".to_string(),
            oracle(|id| day_of(id).is_some_and(|d| d >= 14)),
        ),
        (
            "lower(tag) = 'ja' AND kind = 'a'".to_string(),
            oracle(|id| lower_tag_is(id, "ja") && id.is_multiple_of(2)),
        ),
        (
            "lower(tag) = 'ja' AND qty + 1 > 5".to_string(),
            oracle(|id| lower_tag_is(id, "ja") && qty_plus_one(id).is_some_and(|v| v > 5.0)),
        ),
    ]
}

fn create_expression_indexes(core: &EngineCore) {
    ddl(core, "CREATE INDEX i_lower ON docs ((lower(tag)))");
    ddl(core, "CREATE INDEX i_qty ON docs ((qty + 1))");
    ddl(core, "CREATE INDEX i_day ON docs ((date_trunc('day', ts)))");
}

fn without_private(v: Vec<u64>) -> Vec<u64> {
    v.into_iter().filter(|id| *id != PRIVATE_ID).collect()
}

/// 宣言前・宣言後・`DROP INDEX` 後で結果が全走査オラクルと一致し、宣言後は索引経路が実際に使われる。
#[test]
fn expression_predicates_use_index_and_match_oracle() {
    let (core, _guard) = open_core("expr-planner-use");

    // 宣言前は従来どおりの全走査（索引経路は発火しない）。
    for (clause, expected) in cases() {
        let before = core.scalar_index_cache_stats().index_scans;
        let got = without_private(ids(&core, "tenant-a", &clause));
        assert_eq!(got, expected, "pre-declaration oracle mismatch: {clause}");
        assert_eq!(
            core.scalar_index_cache_stats().index_scans,
            before,
            "no index may be consumed before declaration: {clause}"
        );
    }

    create_expression_indexes(&core);
    for (clause, expected) in cases() {
        // 1 回目で索引を構築・登録し、2 回目で消費する。
        let _ = ids(&core, "tenant-a", &clause);
        let before = core.scalar_index_cache_stats();
        let got = without_private(ids(&core, "tenant-a", &clause));
        let after = core.scalar_index_cache_stats();
        assert_eq!(got, expected, "oracle mismatch: {clause}");
        assert!(
            after.index_scans > before.index_scans,
            "expression index not consumed (vacuous match): {clause}"
        );
    }

    ddl(&core, "DROP INDEX i_lower");
    ddl(&core, "DROP INDEX i_qty");
    ddl(&core, "DROP INDEX i_day");
    for (clause, expected) in cases() {
        let _ = ids(&core, "tenant-a", &clause);
        let before = core.scalar_index_cache_stats().index_scans;
        let got = without_private(ids(&core, "tenant-a", &clause));
        assert_eq!(got, expected, "post-drop oracle mismatch: {clause}");
        assert_eq!(
            core.scalar_index_cache_stats().index_scans,
            before,
            "dropped index must not be consumed: {clause}"
        );
    }
}

/// 一致しない形・残余述語が混ざる形・型違いのリテラルは全走査のまま（結果は同じ）。
#[test]
fn non_matching_shapes_fall_back_to_plain_scan() {
    let (core, _guard) = open_core("expr-planner-nomatch");
    ddl(&core, "CREATE INDEX i_lower ON docs ((lower(tag)))");
    ddl(&core, "CREATE INDEX i_qty ON docs ((qty + 1))");
    let cases: Vec<(&str, Vec<u64>)> = vec![
        // 別の式（宣言は lower）。
        (
            "upper(tag) = 'JA'",
            oracle(|id| tag_of(id).is_some_and(|t| t.to_uppercase() == "JA")),
        ),
        // 引数違い（宣言は qty + 1）。
        (
            "qty + 2 > 5",
            oracle(|id| qty_of(id).is_some_and(|q| f64::from(q) + 2.0 > 5.0)),
        ),
        // 残余述語（宣言外の式）が混ざる。
        (
            "lower(tag) = 'ja' AND upper(tag) = 'JA'",
            oracle(|id| {
                lower_tag_is(id, "ja") && tag_of(id).is_some_and(|t| t.to_uppercase() == "JA")
            }),
        ),
        // 式に対する <> は対象外。
        (
            "lower(tag) <> 'ja'",
            oracle(|id| tag_of(id).is_some_and(|t| t.to_lowercase() != "ja")),
        ),
    ];
    for (clause, expected) in cases {
        let _ = ids(&core, "tenant-a", clause);
        let before = core.scalar_index_cache_stats().index_scans;
        let got = without_private(ids(&core, "tenant-a", clause));
        assert_eq!(got, expected, "oracle mismatch: {clause}");
        assert_eq!(
            core.scalar_index_cache_stats().index_scans,
            before,
            "must not consume an index: {clause}"
        );
    }
}

/// opt-in なし（HNSW 宣言を有効化していないエンジン）では式索引を使わず、EXPLAIN にも出ない。
#[test]
fn engine_without_declarations_never_uses_expression_index() {
    let (storage, _guard) = seed("expr-planner-optout");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider))
        .with_query_planner(Box::new(StubLlmClient));
    ddl(&core, "CREATE INDEX i_lower ON docs ((lower(tag)))");
    let clause = "lower(tag) = 'ja'";
    let _ = ids(&core, "tenant-a", clause);
    let got = without_private(ids(&core, "tenant-a", clause));
    assert_eq!(got, oracle(|id| lower_tag_is(id, "ja")));
    assert_eq!(core.scalar_index_cache_stats().index_scans, 0);
    assert_eq!(
        scalar_plan(&explain(&core, clause)),
        "scalar_plan: plain_scan"
    );
}

/// 他テナントの `Private` 行は、式が一致しても候補・結果に現れない。
#[test]
fn private_rows_do_not_leak_through_expression_index() {
    let (core, _guard) = open_core("expr-planner-rls");
    create_expression_indexes(&core);
    for clause in ["lower(tag) = 'ja'", "qty + 1 = 4"] {
        let _ = ids(&core, "tenant-a", clause);
        let owner = ids(&core, "tenant-a", clause);
        assert!(
            owner.contains(&PRIVATE_ID),
            "owner must see its row: {clause}"
        );
        let _ = ids(&core, "tenant-b", clause);
        let other = ids(&core, "tenant-b", clause);
        assert!(!other.contains(&PRIVATE_ID), "private row leaked: {clause}");
    }
    let explain_other = explain(&core, "lower(tag) = 'ja'");
    assert!(!explain_other
        .iter()
        .any(|l| l.contains(&PRIVATE_ID.to_string())));
}

/// NULL を返す行は、等価・範囲のどちらでも結果に入らない（全走査と一致）。
#[test]
fn null_expression_rows_are_excluded() {
    let (core, _guard) = open_core("expr-planner-null");
    create_expression_indexes(&core);
    let null_ids = oracle(|id| tag_of(id).is_none());
    assert!(!null_ids.is_empty());
    for clause in [
        "lower(tag) = 'ja'",
        "lower(tag) >= ''",
        "lower(tag) <= 'zzz'",
    ] {
        let _ = ids(&core, "tenant-a", clause);
        let got = ids(&core, "tenant-a", clause);
        for id in &null_ids {
            assert!(!got.contains(id), "NULL row {id} matched: {clause}");
        }
    }
}

/// 式索引の作成前に構築・キャッシュされた索引があっても、作成後は新しい宣言で式一致が効く
/// （`CREATE INDEX` がテーブル世代を進めるため、古い索引は使われない）。
#[test]
fn index_created_after_cached_scalar_index_is_picked_up() {
    let (core, _guard) = open_core("expr-planner-generation");
    ddl(&core, "CREATE INDEX i_kind ON docs (kind)");
    let warm = "kind = 'a' AND id <= 300";
    let _ = ids(&core, "tenant-a", warm);
    let _ = ids(&core, "tenant-a", warm);
    assert!(core.scalar_index_cache_stats().index_scans > 0);

    ddl(&core, "CREATE INDEX i_lower ON docs ((lower(tag)))");
    let clause = "lower(tag) = 'ja'";
    let _ = ids(&core, "tenant-a", clause);
    let before = core.scalar_index_cache_stats().index_scans;
    let got = without_private(ids(&core, "tenant-a", clause));
    assert_eq!(got, oracle(|id| lower_tag_is(id, "ja")));
    assert!(core.scalar_index_cache_stats().index_scans > before);
}

/// EXPLAIN は一致時に `index=<式索引名>` を出し、不一致では plain_scan（索引名なし）。
/// 検索 EXPLAIN・集計 EXPLAIN・`USING PLAN` 付き EXPLAIN の 3 経路で確認する。
#[test]
fn explain_reports_expression_index_names() {
    let (core, _guard) = open_core("expr-planner-explain");
    create_expression_indexes(&core);
    ddl(&core, "CREATE INDEX i_kind ON docs (kind)");

    // 検索 EXPLAIN。
    assert_eq!(
        scalar_plan(&explain(&core, "lower(tag) = 'ja'")),
        "scalar_plan: index_equality index=i_lower"
    );
    assert_eq!(
        scalar_plan(&explain(&core, "qty + 1 > 5")),
        "scalar_plan: index_typed_range index=i_qty"
    );
    assert_eq!(
        scalar_plan(&explain(
            &core,
            "date_trunc('day', ts) = TIMESTAMP '1970-01-05 00:00:00'"
        )),
        "scalar_plan: index_typed_range index=i_day"
    );
    assert_eq!(
        scalar_plan(&explain(&core, "lower(tag) = 'ja' AND qty + 1 > 5")),
        "scalar_plan: index_conjunction index=i_lower,i_qty"
    );
    assert_eq!(
        scalar_plan(&explain(&core, "lower(tag) = 'ja' AND kind = 'a'")),
        "scalar_plan: index_conjunction index=i_kind,i_lower"
    );
    // 不一致・残余述語入りは plain_scan（索引名なし）。
    assert_eq!(
        scalar_plan(&explain(&core, "upper(tag) = 'JA'")),
        "scalar_plan: plain_scan"
    );
    assert_eq!(
        scalar_plan(&explain(&core, "lower(tag) = 'ja' AND upper(tag) = 'JA'")),
        "scalar_plan: plain_scan"
    );

    // 集計 EXPLAIN。
    let agg = explain_sql(
        &core,
        "EXPLAIN SELECT COUNT(*) FROM docs WHERE lower(tag) = 'ja'",
    );
    assert_eq!(
        scalar_plan(&agg),
        "scalar_plan: index_equality index=i_lower"
    );
    let agg_miss = explain_sql(
        &core,
        "EXPLAIN SELECT COUNT(*) FROM docs WHERE upper(tag) = 'JA'",
    );
    assert_eq!(scalar_plan(&agg_miss), "scalar_plan: plain_scan");

    // USING PLAN の EXPLAIN。
    let plan = explain_sql(
        &core,
        "EXPLAIN SELECT id FROM docs WHERE lower(tag) = 'ja' USING PLAN('find content') LIMIT 5",
    );
    assert_eq!(
        scalar_plan(&plan),
        "scalar_plan: index_equality index=i_lower"
    );
    let plan_miss = explain_sql(
        &core,
        "EXPLAIN SELECT id FROM docs WHERE upper(tag) = 'JA' USING PLAN('find content') LIMIT 5",
    );
    assert_eq!(scalar_plan(&plan_miss), "scalar_plan: plain_scan");
}

/// 式索引を `DROP INDEX` すると EXPLAIN の索引名も消える。
#[test]
fn explain_drops_name_after_drop_index() {
    let (core, _guard) = open_core("expr-planner-explain-drop");
    ddl(&core, "CREATE INDEX i_lower ON docs ((lower(tag)))");
    assert_eq!(
        scalar_plan(&explain(&core, "lower(tag) = 'ja'")),
        "scalar_plan: index_equality index=i_lower"
    );
    ddl(&core, "DROP INDEX i_lower");
    assert_eq!(
        scalar_plan(&explain(&core, "lower(tag) = 'ja'")),
        "scalar_plan: plain_scan"
    );
}
fn aggregate_rows(core: &EngineCore, sql: &str) -> Vec<Vec<Cell>> {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("aggregate must succeed: {sql}: {e:?}"))
        .rows
        .iter()
        .map(|r| r.cells.clone())
        .collect()
}

/// 式述語だけの集計（`COUNT(*)` は候補数をそのまま件数にする信頼経路）と `GROUP BY` の候補走査形も、
/// 索引経路を実際に使いつつ全走査オラクルと一致する。`coalesce` を含む式も対象にする。
#[test]
fn aggregates_over_expression_predicates_match_oracle() {
    let (core, _guard) = open_core("expr-planner-aggregate");
    ddl(&core, "CREATE INDEX i_lower ON docs ((lower(tag)))");
    ddl(&core, "CREATE INDEX i_co ON docs ((coalesce(tag, 'zz')))");

    // 公開行 + tenant-a 視点で見える Private 行（`lower(tag) = 'ja'` に一致）を数える。
    let ja = oracle(|id| lower_tag_is(id, "ja")).len() as u64 + 1;
    let count_sql = "SELECT COUNT(*) AS n FROM docs WHERE lower(tag) = 'ja'";
    let _ = aggregate_rows(&core, count_sql);
    let before = core.scalar_index_cache_stats().aggregate_index_scans;
    assert_eq!(
        aggregate_rows(&core, count_sql),
        vec![vec![Cell::Integer(ja)]]
    );
    assert!(
        core.scalar_index_cache_stats().aggregate_index_scans > before,
        "aggregate must consume the expression index"
    );

    // NULL を `coalesce` で畳んだ式（NULL 行は 'zz' になる）。
    let zz = oracle(|id| tag_of(id).is_none()).len() as u64;
    let co_sql = "SELECT COUNT(*) AS n FROM docs WHERE coalesce(tag, 'zz') = 'zz'";
    let _ = aggregate_rows(&core, co_sql);
    let before = core.scalar_index_cache_stats().aggregate_index_scans;
    assert_eq!(aggregate_rows(&core, co_sql), vec![vec![Cell::Integer(zz)]]);
    assert!(core.scalar_index_cache_stats().aggregate_index_scans > before);

    // GROUP BY の候補走査形。
    let group_sql =
        "SELECT kind, COUNT(*) AS n FROM docs WHERE lower(tag) = 'ja' GROUP BY kind ORDER BY kind";
    let _ = aggregate_rows(&core, group_sql);
    let before = core.scalar_index_cache_stats().aggregate_index_scans;
    let got = aggregate_rows(&core, group_sql);
    let a = oracle(|id| lower_tag_is(id, "ja") && id.is_multiple_of(2)).len() as u64 + 1;
    let b = oracle(|id| lower_tag_is(id, "ja") && !id.is_multiple_of(2)).len() as u64;
    assert_eq!(
        got,
        vec![
            vec![Cell::Text("a".to_string()), Cell::Integer(a)],
            vec![Cell::Text("b".to_string()), Cell::Integer(b)],
        ]
    );
    assert!(core.scalar_index_cache_stats().aggregate_index_scans > before);
}
