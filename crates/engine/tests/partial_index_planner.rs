//! 部分索引のプランナ選択と EXPLAIN 表示（INDEX-11、Issue #1656）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-11・INDEX-7。
//!
//! `tests/expression_index_planner.rs` と同じ構成（HNSW opt-in の宣言付きエンジン・
//! `MIN_INDEXED_ROWS` 超の行数・他テナントの `Private` 行）で、`WHERE` が部分索引の述語を含意する
//! ときだけ部分索引の格納行で候補を絞ること、結果が全走査のオラクルと一致すること（残りの述語は
//! 必ず再評価される）、EXPLAIN が `scalar_plan: index_partial index=<索引名>` を示すこと、含意しない
//! 形・opt-in なし・集計では従来どおりであることを固定する。索引経路が実際に発火したことは
//! `ScalarIndexCacheStats` で確かめ、信頼マスク経路へ入っていないこと（述語外の行が混ざる fail-open
//! の回避）も同じ統計で固定する。

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
/// 残余述語（宣言のない式）。これが混ざると通常の分類は `PlainScan` になるため、索引経路の発火は
/// 部分索引由来に限られる。
const RESIDUAL: &str = "lower(tag) = 'ja'";

fn without_private(v: Vec<u64>) -> Vec<u64> {
    v.into_iter().filter(|id| *id != PRIVATE_ID).collect()
}

/// `qty >= 3`（NULL は偽）。
fn q3(id: u64) -> bool {
    qty_of(id).is_some_and(|q| q >= 3)
}

fn is_even(id: u64) -> bool {
    id.is_multiple_of(2)
}

/// 2 回実行し（1 回目で構築）、2 回目に部分索引経路が使われ、信頼マスクには入らないことを確かめる。
fn assert_partial_consumed(core: &EngineCore, clause: &str, expected: &[u64]) {
    let _ = ids(core, "tenant-a", clause);
    let before = core.scalar_index_cache_stats();
    let got = without_private(ids(core, "tenant-a", clause));
    let after = core.scalar_index_cache_stats();
    assert_eq!(got, expected, "oracle mismatch: {clause}");
    assert!(
        after.index_scans > before.index_scans,
        "partial index not consumed (vacuous): {clause}"
    );
    assert_eq!(
        after.index_trusted_mask_scans, before.index_trusted_mask_scans,
        "partial candidates must never take the trusted-mask path: {clause}"
    );
}

fn assert_not_consumed(core: &EngineCore, clause: &str, expected: &[u64]) {
    let _ = ids(core, "tenant-a", clause);
    let before = core.scalar_index_cache_stats().index_scans;
    let got = without_private(ids(core, "tenant-a", clause));
    assert_eq!(got, expected, "oracle mismatch: {clause}");
    assert_eq!(
        core.scalar_index_cache_stats().index_scans,
        before,
        "no index may be consumed: {clause}"
    );
}

/// 含意する形では部分索引を使い、結果は全走査オラクルと一致し、EXPLAIN は `index_partial` を示す。
#[test]
fn implied_queries_use_partial_index_and_match_oracle() {
    let (core, _guard) = open_core("partial-planner-use");
    ddl(
        &core,
        "CREATE INDEX p_kind ON docs (path) WHERE kind = 'a' AND qty > 2",
    );
    ddl(
        &core,
        "CREATE INDEX p_tag ON docs (path) WHERE tag IS NOT NULL AND qty > 3",
    );
    ddl(&core, "CREATE INDEX p_qty ON docs (path) WHERE qty > 5");
    ddl(
        &core,
        "CREATE INDEX p_btw ON docs (path) WHERE qty BETWEEN 5 AND 10",
    );
    ddl(
        &core,
        "CREATE INDEX p_in ON docs (path) WHERE kind IN ('a', 'c') AND qty > 4",
    );

    let ja = |id: u64| lower_tag_is(id, "ja");
    let q = |id: u64| qty_of(id);
    let cases: Vec<(String, Vec<u64>, &str)> = vec![
        (
            format!("kind = 'a' AND qty >= 6 AND {RESIDUAL}"),
            oracle(|id| is_even(id) && q(id).is_some_and(|v| v >= 6) && ja(id)),
            "p_in,p_kind,p_qty",
        ),
        (
            format!("tag = 'Ja' AND qty >= 5 AND {RESIDUAL}"),
            oracle(|id| tag_of(id) == Some("Ja") && q(id).is_some_and(|v| v >= 5) && ja(id)),
            "p_tag",
        ),
        (
            format!("qty >= 7 AND {RESIDUAL}"),
            oracle(|id| q(id).is_some_and(|v| v >= 7) && ja(id)),
            "p_qty",
        ),
        (
            format!("qty BETWEEN 6 AND 7 AND {RESIDUAL}"),
            oracle(|id| q(id).is_some_and(|v| (6..=7).contains(&v)) && ja(id)),
            "p_btw,p_qty",
        ),
    ];
    for (clause, expected, names) in &cases {
        assert_partial_consumed(&core, clause, expected);
        assert_eq!(
            scalar_plan(&explain(&core, clause)),
            format!("scalar_plan: index_partial index={names}"),
            "{clause}"
        );
    }
    // `USING PLAN` 経路の `scalar_plan:` も検索経路と同じになる。
    let using = explain_sql(
        &core,
        &format!(
            "EXPLAIN SELECT id FROM docs WHERE kind = 'a' AND qty >= 6 AND {RESIDUAL} \
             USING PLAN('hello') LIMIT 10"
        ),
    );
    assert_eq!(
        scalar_plan(&using),
        "scalar_plan: index_partial index=p_in,p_kind,p_qty"
    );
}

/// 含意しない形（弱い条件・OR の中だけに根拠がある・述語の列に触れない）は部分索引を使わない。
#[test]
fn non_implied_queries_never_use_partial_index() {
    let (core, _guard) = open_core("partial-planner-nomatch");
    ddl(&core, "CREATE INDEX p_qty ON docs (path) WHERE qty > 5");
    ddl(
        &core,
        "CREATE INDEX p_kind ON docs (path) WHERE kind = 'a' AND qty > 2",
    );
    let ja = |id: u64| lower_tag_is(id, "ja");
    let cases: Vec<(String, Vec<u64>)> = vec![
        // P は `qty > 5`。`qty > 4` は含意しない。
        (
            format!("qty > 4 AND {RESIDUAL}"),
            oracle(|id| qty_of(id).is_some_and(|q| q > 4) && ja(id)),
        ),
        // 含意の根拠が `OR` の中だけにある。
        (
            format!("(kind = 'a' OR kind = 'b') AND {RESIDUAL}"),
            oracle(ja),
        ),
        // 述語の列に触れない。
        (RESIDUAL.to_string(), oracle(ja)),
    ];
    for (clause, expected) in &cases {
        assert_not_consumed(&core, clause, expected);
        let line = scalar_plan(&explain(&core, clause));
        assert!(!line.contains("index_partial"), "{clause}: {line}");
        assert!(
            !line.contains("p_qty") && !line.contains("p_kind"),
            "{clause}: {line}"
        );
    }
}

/// 通常の分類が `PlainScan` でない形（残余なしの索引対応述語）は従来の索引経路のまま。
#[test]
fn regular_index_path_is_preferred_over_partial_index() {
    let (core, _guard) = open_core("partial-planner-regular");
    ddl(
        &core,
        "CREATE INDEX p_kind ON docs (path) WHERE kind = 'a' AND qty > 2",
    );
    let clause = "kind = 'a'";
    let _ = ids(&core, "tenant-a", clause);
    let got = without_private(ids(&core, "tenant-a", clause));
    assert_eq!(got, oracle(is_even));
    let line = scalar_plan(&explain(&core, clause));
    assert!(!line.contains("index_partial"), "{line}");
    assert!(!line.contains("p_kind"), "{line}");
}

/// opt-in なし（宣言が無効なエンジン）では部分索引を使わず、EXPLAIN にも出ない。
#[test]
fn engine_without_declarations_never_uses_partial_index() {
    let (storage, _guard) = seed("partial-planner-optin");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider))
        .with_query_planner(Box::new(StubLlmClient));
    ddl(
        &core,
        "CREATE INDEX p_kind ON docs (path) WHERE kind = 'a' AND qty > 2",
    );
    let clause = format!("kind = 'a' AND qty >= 3 AND {RESIDUAL}");
    let expected = oracle(|id| is_even(id) && q3(id) && lower_tag_is(id, "ja"));
    let got = without_private(ids(&core, "tenant-a", &clause));
    assert_eq!(got, expected);
    assert_eq!(core.scalar_index_cache_stats().index_scans, 0);
    assert_eq!(
        scalar_plan(&explain(&core, &clause)),
        "scalar_plan: plain_scan"
    );
}

/// 他テナントの Private 行は、部分索引（述語を満たす行を含む）経由でも結果・EXPLAIN に出ない。
#[test]
fn private_rows_do_not_leak_through_partial_index() {
    let (core, _guard) = open_core("partial-planner-rls");
    ddl(
        &core,
        "CREATE INDEX p_kind ON docs (path) WHERE kind = 'a' AND qty > 2",
    );
    let clause = format!("kind = 'a' AND qty >= 3 AND {RESIDUAL}");
    let _ = ids(&core, "tenant-a", &clause);
    let owner = ids(&core, "tenant-a", &clause);
    assert!(owner.contains(&PRIVATE_ID), "owner must see its row");
    let _ = ids(&core, "tenant-b", &clause);
    let other = ids(&core, "tenant-b", &clause);
    assert!(!other.contains(&PRIVATE_ID), "private row leaked");
    assert_eq!(without_private(other), without_private(owner));
    assert!(!explain(&core, &clause)
        .iter()
        .any(|l| l.contains(&PRIVATE_ID.to_string())));
}

/// 行が述語の内外へ移っても（UPDATE）、結果は全走査オラクルと一致する（世代ごとの再構築）。
#[test]
fn rows_moving_in_and_out_of_the_predicate_stay_consistent() {
    let (core, _guard) = open_core("partial-planner-move");
    ddl(
        &core,
        "CREATE INDEX p_kind ON docs (path) WHERE kind = 'a' AND qty > 2",
    );
    let clause = format!("kind = 'a' AND qty >= 3 AND {RESIDUAL}");
    assert_partial_consumed(
        &core,
        &clause,
        &oracle(|id| is_even(id) && q3(id) && lower_tag_is(id, "ja")),
    );
    // id 5（奇数。kind = 'b'）を述語の内側へ、id 10（偶数）を外側へ移す。
    for sql in [
        "UPDATE docs SET kind = 'a', tag = 'ja', qty = 6 WHERE id = 5 USING OPERATION_ID 'op-in'",
        "UPDATE docs SET kind = 'b' WHERE id = 10 USING OPERATION_ID 'op-out'",
    ] {
        let mut session = SessionState::default();
        core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
            .unwrap_or_else(|e| panic!("update must succeed: {sql}: {e:?}"));
    }
    let expected = oracle(|id| match id {
        5 => true,
        10 => false,
        _ => is_even(id) && q3(id) && lower_tag_is(id, "ja"),
    });
    assert_partial_consumed(&core, &clause, &expected);
}

/// 集計は部分索引を使わず、EXPLAIN にも名前を出さない（範囲は検索 SELECT のみ）。
#[test]
fn aggregates_do_not_use_partial_index() {
    let (core, _guard) = open_core("partial-planner-aggregate");
    ddl(
        &core,
        "CREATE INDEX p_kind ON docs (path) WHERE kind = 'a' AND qty > 2",
    );
    let sql =
        format!("SELECT COUNT(*) AS n FROM docs WHERE kind = 'a' AND qty >= 3 AND {RESIDUAL}");
    let expected = oracle(|id| is_even(id) && q3(id) && lower_tag_is(id, "ja")).len() as u64 + 1;
    for _ in 0..2 {
        let rows = core
            .execute_sql(&ctx("tenant-a"), &sql)
            .expect("aggregate must succeed")
            .rows;
        assert_eq!(
            rows.iter().map(|r| r.cells.clone()).collect::<Vec<_>>(),
            vec![vec![Cell::Integer(expected)]]
        );
    }
    let lines = explain_sql(&core, &format!("EXPLAIN {sql}"));
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("index_partial") || l.contains("p_kind")),
        "{lines:?}"
    );
}
