//! `WITH RECURSIVE` の重複除去版 `UNION` と循環の停止の結合テスト（SQL-35、Issue #1872）。
//! ポインタ: `docs/spec/04-behavior/` の SQL-35・SQL-25 (c)・SQL-29 (c)・SQL-34・RLS-10 (b)・ERR-6。
//!
//! `sql35_recursive_cte_eval.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）。
//! 期待値は PostgreSQL 17 の RecursiveUnion の意味から手計算で固定する（捨てた行は結果にも
//! 次の作業表にも入らず、出力順は生成順）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::recursive_cte_limits::RecursiveCteLimits;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn edges_schema() -> TableSchema {
    TableSchema::new(
        "edges",
        vec![
            ColumnDef::new("src", ColumnType::BigInt, false),
            ColumnDef::new("dst", ColumnType::BigInt, false),
        ],
    )
}

fn put_edge(storage: &Storage, tenant: &str, row_id: u64, src: i64, dst: i64, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        "edges",
        &ctx(tenant),
        row_id,
        vis,
        &[Value::BigInt(src), Value::BigInt(dst)],
        &OperationId::parse(&format!("edge-{tenant}-{row_id}")).expect("operation id"),
    )
    .expect("insert edge");
}

fn put_parent(storage: &Storage, row_id: u64, parent: Option<i64>) {
    engine::tenant::insert_typed_row(
        storage,
        "tree",
        &ctx("alice"),
        row_id,
        Visibility::Public,
        &[
            Value::BigInt(row_id as i64),
            match parent {
                Some(p) => Value::BigInt(p),
                None => Value::Null,
            },
        ],
        &OperationId::parse(&format!("node-{row_id}")).expect("operation id"),
    )
    .expect("insert node");
}

fn open_core_with(
    tag: &str,
    limits: Option<RecursiveCteLimits>,
    seed: impl FnOnce(&Storage),
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&edges_schema()).expect("create edges");
    storage
        .create_table(&TableSchema::new(
            "tree",
            vec![
                ColumnDef::new("node_id", ColumnType::BigInt, false),
                ColumnDef::new("parent_id", ColumnType::BigInt, true),
            ],
        ))
        .expect("create tree");
    storage
        .create_table(&TableSchema::new(
            "vt",
            vec![ColumnDef::new("v", ColumnType::Vector(2), false)],
        ))
        .expect("create vt");
    storage
        .create_table(&TableSchema::new(
            "jt",
            vec![ColumnDef::new("j", ColumnType::Json, true)],
        ))
        .expect("create jt");
    seed(&storage);
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(l) = limits {
        core = core.with_recursive_cte_limits(l);
    }
    (core, guard)
}

/// 循環 1 → 2 → 3 → 1。
fn seed_cycle(storage: &Storage) {
    for (i, (s, d)) in [(1, 2), (2, 3), (3, 1)].iter().enumerate() {
        put_edge(storage, "alice", i as u64 + 1, *s, *d, Visibility::Public);
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .expect_err("must be rejected")
        .wire_code()
        .to_string()
}

fn text(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Float(v) => v.to_string(),
        Cell::Text(s) => s.clone(),
        other => panic!("unexpected cell {other:?}"),
    }
}

fn col(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    run(core, tenant, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
        .iter()
        .map(|r| r.cells.iter().map(text).collect::<Vec<_>>().join(","))
        .collect()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn walk(op: &str) -> String {
    format!(
        "WITH RECURSIVE r(n) AS (SELECT CAST(1 AS BIGINT) {op} SELECT e.dst FROM edges e JOIN r ON e.src = r.n) \
         SELECT n FROM r"
    )
}

/// JOIN の再帰項: 循環グラフでも `UNION` は止まり、生成順で 1 回ずつ返す。`UNION ALL` は上限で `54000`。
#[test]
fn join_term_cycle_stops_with_union() {
    let (core, _g) = open_core_with("sql35-union-cycle", None, seed_cycle);
    assert_eq!(col(&core, "alice", &walk("UNION")), strs(&["1", "2", "3"]));
    assert_eq!(
        col(&core, "alice", &walk("UNION DISTINCT")),
        strs(&["1", "2", "3"])
    );
    assert_eq!(code(&core, "alice", &walk("UNION ALL")), "54000");
}

/// Scan の再帰項の循環（1 → 3 → 1）も `UNION` で止まる。
#[test]
fn scan_term_cycle_stops_with_union() {
    let (core, _g) = open_core_with("sql35-union-scan", None, |_| {});
    let sql = "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT 4 - n FROM t) SELECT n FROM t";
    assert_eq!(col(&core, "alice", sql), strs(&["1", "3"]));
}

/// ひし形（1→2, 1→3, 2→4, 3→4）: 同じ反復で出た重複 4 は `UNION` で 1 回、`UNION ALL` で 2 回。
#[test]
fn same_iteration_duplicates_are_removed() {
    let (core, _g) = open_core_with("sql35-union-diamond", None, |s| {
        for (i, (a, b)) in [(1, 2), (1, 3), (2, 4), (3, 4)].iter().enumerate() {
            put_edge(s, "alice", i as u64 + 1, *a, *b, Visibility::Public);
        }
    });
    assert_eq!(
        col(&core, "alice", &walk("UNION")),
        strs(&["1", "2", "3", "4"])
    );
    assert_eq!(
        col(&core, "alice", &walk("UNION ALL")),
        strs(&["1", "2", "3", "4", "4"])
    );
}

/// アンカーの中の重複も除く。NULL 同士は等しい（NULL は 1 行）。
#[test]
fn anchor_duplicates_and_nulls_are_collapsed() {
    let (core, _g) = open_core_with("sql35-union-anchor", None, |s| {
        // 親: NULL, 1, 1, 2, NULL
        for (i, p) in [None, Some(1), Some(1), Some(2), None].iter().enumerate() {
            put_parent(s, i as u64 + 1, *p);
        }
    });
    let q = |op: &str| {
        format!(
            "WITH RECURSIVE t(n) AS (SELECT parent_id FROM tree {op} \
             SELECT n FROM t WHERE n < 0) SELECT count(*) FROM t"
        )
    };
    assert_eq!(col(&core, "alice", &q("UNION")), strs(&["3"]));
    assert_eq!(col(&core, "alice", &q("UNION ALL")), strs(&["5"]));
}

/// 捨てた重複は行数上限に数えない: 重複除去後の行数（3）ちょうどの上限は成功、2 は `54000`。
#[test]
fn duplicates_do_not_count_toward_row_limit() {
    let (core, _g) = open_core_with(
        "sql35-union-rows-ok",
        Some(RecursiveCteLimits::new(1000, 3)),
        seed_cycle,
    );
    assert_eq!(col(&core, "alice", &walk("UNION")), strs(&["1", "2", "3"]));
    let (core, _g2) = open_core_with(
        "sql35-union-rows-over",
        Some(RecursiveCteLimits::new(1000, 2)),
        seed_cycle,
    );
    assert_eq!(code(&core, "alice", &walk("UNION")), "54000");
}

/// JOIN の候補行（重複除去前）には最終結果の行数上限を適用しない: 同じ行を 4 件返す JOIN でも、
/// 重複除去後（2 行）が `--max-result-rows=3` 以内なら成功する。
#[test]
fn join_candidates_before_dedup_do_not_hit_result_row_limit() {
    let (core, _g) = open_core_with(
        "sql35-union-join-candidates",
        Some(RecursiveCteLimits::new(1000, 100)),
        |storage| {
            for i in 0..4u64 {
                put_edge(storage, "alice", i + 1, 1, 2, Visibility::Public);
            }
        },
    );
    let core = core.with_result_limits(engine::result_limits::ResultLimits::new(3));
    assert_eq!(col(&core, "alice", &walk("UNION")), strs(&["1", "2"]));
}

/// アンカー候補にも最終結果の行数上限を適用しない: 同じ値を 4 行返すアンカーでも、重複除去後
/// （1 行）が `--max-result-rows=3` 以内なら成功する。
#[test]
fn anchor_candidates_before_dedup_do_not_hit_result_row_limit() {
    let (core, _g) = open_core_with(
        "sql35-union-anchor-candidates",
        Some(RecursiveCteLimits::new(1000, 100)),
        |s| {
            for i in 0..4u64 {
                put_parent(s, i + 1, Some(7));
            }
        },
    );
    let core = core.with_result_limits(engine::result_limits::ResultLimits::new(3));
    let sql = "WITH RECURSIVE t(n) AS (SELECT parent_id FROM tree UNION \
               SELECT n FROM t WHERE n < 0) SELECT n FROM t";
    assert_eq!(col(&core, "alice", sql), strs(&["7"]));
}

/// 重複が多くても、重複除去後の行数が上限を超えれば部分結果を返さず `54000`。
#[test]
fn distinct_rows_over_limit_fail_closed() {
    let (core, _g) = open_core_with(
        "sql35-union-unbounded",
        Some(RecursiveCteLimits::new(1000, 50)),
        |_| {},
    );
    let sql = "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT n + 1 FROM t) SELECT n FROM t";
    assert_eq!(code(&core, "alice", sql), "54000");
}

/// 型ゲート: `VECTOR` 列は集合演算 `UNION` と同じ `22000`、`JSON` 列は `0A000`。`UNION ALL` は受理。
#[test]
fn types_without_equality_are_rejected_for_union() {
    let (core, _g) = open_core_with("sql35-union-types", None, |_| {});
    let v = |op: &str| {
        format!("WITH RECURSIVE t(v) AS (SELECT v FROM vt {op} SELECT v FROM t) SELECT v FROM t")
    };
    // `VECTOR` 列は再帰項の束縛段で既に参照できず、`UNION` でも `UNION ALL` でも拒否される（fail-closed）。
    assert!(run(&core, "alice", &v("UNION")).is_err());
    assert!(run(&core, "alice", &v("UNION ALL")).is_err());
    let j = |op: &str| {
        format!("WITH RECURSIVE t(j) AS (SELECT j FROM jt {op} SELECT j FROM t) SELECT j FROM t")
    };
    assert_eq!(code(&core, "alice", &j("UNION")), "0A000");
}

/// `SEARCH` 句と `UNION` の併用は（順序列を含めた重複判定が未対応のため）固定の `0A000`。
#[test]
fn search_with_union_is_gated() {
    let (core, _g) = open_core_with("sql35-union-search", None, seed_cycle);
    let sql =
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT e.dst FROM edges e JOIN r ON e.src = r.n) \
               SEARCH DEPTH FIRST BY n SET o SELECT n FROM r";
    assert_eq!(code(&core, "alice", sql), "0A000");
}

/// Describe（拡張クエリ）は Execute と同じ列・同じエラーを返す。
#[test]
fn describe_matches_execute() {
    let (core, _g) = open_core_with("sql35-union-describe", None, seed_cycle);
    let sql = walk("UNION");
    let session = SessionState::default();
    let parsed = core.parse_sql(&sql).expect("parse");
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe")
        .expect("has columns");
    assert_eq!(described, run(&core, "alice", &sql).expect("run").columns);
    let bad = "WITH RECURSIVE t(j) AS (SELECT j FROM jt UNION SELECT j FROM t) SELECT j FROM t";
    let parsed = core.parse_sql(bad).expect("parse");
    let err = core
        .describe_parsed_in_session(&session, &parsed)
        .expect_err("describe must fail");
    assert_eq!(err.wire_code(), "0A000");
}

/// RLS-10 (b): 他テナントの Private な辺（循環を閉じる辺・枝）は、止まる時点・行数に影響しない。
#[test]
fn other_tenant_rows_do_not_affect_termination() {
    let (core, _g) = open_core_with("sql35-union-rls", None, |s| {
        put_edge(s, "alice", 1, 1, 2, Visibility::Private);
        put_edge(s, "alice", 2, 2, 3, Visibility::Private);
        put_edge(s, "bob", 3, 3, 1, Visibility::Private);
        put_edge(s, "bob", 4, 1, 9, Visibility::Private);
    });
    // alice からは 3 → 1 も 1 → 9 も不可視: 1, 2, 3 で終わる（循環は閉じない）。
    assert_eq!(col(&core, "alice", &walk("UNION")), strs(&["1", "2", "3"]));
    assert_eq!(
        col(&core, "alice", &walk("UNION ALL")),
        strs(&["1", "2", "3"])
    );
    // bob からは alice の行が不可視: 1, 9 で終わる。
    assert_eq!(col(&core, "bob", &walk("UNION")), strs(&["1", "9"]));
}
