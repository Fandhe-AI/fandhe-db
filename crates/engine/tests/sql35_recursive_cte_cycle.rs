//! `WITH RECURSIVE` の `CYCLE ... SET ... USING` 句の結合テスト（SQL-35、Issue #2214）。
//! ポインタ: `docs/spec/04-behavior/` の SQL-35・RLS-10 (b)。
//!
//! `sql35_recursive_cte_search.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）。
//! 期待値は PostgreSQL 17 の書き換え意味（行ごとの経路 `record[]`・`ROW(cols) = ANY(親の経路)` で印を付け、
//! 印が `TO` 値の行は出力するが次の反復へ渡さない）から手計算で固定する。経路は
//! `engine::sql::record_text` の引用規則の文字列、印は boolean を `t`／`f` へ整形して比較する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::record_text::{record_array_out, record_out};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TREE: &str = "tree";
const EDGES: &str = "edges";

fn tree_schema() -> TableSchema {
    TableSchema::new(
        TREE,
        vec![
            ColumnDef::new("node_id", ColumnType::BigInt, false),
            ColumnDef::new("parent_id", ColumnType::BigInt, true),
            ColumnDef::new("name", ColumnType::Text, false),
        ],
    )
}

fn edges_schema() -> TableSchema {
    TableSchema::new(
        EDGES,
        vec![
            ColumnDef::new("src", ColumnType::BigInt, false),
            ColumnDef::new("dst", ColumnType::BigInt, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn put_node(
    storage: &Storage,
    tenant: &str,
    row_id: u64,
    node: i64,
    parent: Option<i64>,
    name: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TREE,
        &ctx(tenant),
        row_id,
        visibility,
        &[
            Value::BigInt(node),
            match parent {
                Some(p) => Value::BigInt(p),
                None => Value::Null,
            },
            Value::Text(name.to_string()),
        ],
        &op_id(&format!("seed-{tenant}-{row_id}")),
    )
    .expect("insert node");
}

fn put_edge(storage: &Storage, row_id: u64, src: i64, dst: i64) {
    engine::tenant::insert_typed_row(
        storage,
        EDGES,
        &ctx("alice"),
        row_id,
        Visibility::Public,
        &[Value::BigInt(src), Value::BigInt(dst)],
        &op_id(&format!("edge-alice-{row_id}")),
    )
    .expect("insert edge");
}

fn open_core_with(seed: impl FnOnce(&Storage)) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql35-recursive-cycle");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&tree_schema()).expect("create table");
    storage.create_table(&edges_schema()).expect("create table");
    seed(&storage);
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn put_edge_as(
    storage: &Storage,
    tenant: &str,
    row_id: u64,
    src: i64,
    dst: i64,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        EDGES,
        &ctx(tenant),
        row_id,
        visibility,
        &[Value::BigInt(src), Value::BigInt(dst)],
        &op_id(&format!("edge-{tenant}-{row_id}")),
    )
    .expect("insert edge");
}

/// 閉路を含む有向グラフ（1 → 2, 2 → 3, 3 → 1, 3 → 4）。
fn seed_cycle(storage: &Storage) {
    for (i, (src, dst)) in [(1, 2), (2, 3), (3, 1), (3, 4)].iter().enumerate() {
        put_edge(storage, (i as u64) + 1, *src, *dst);
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

/// スカラーのセルのテキスト表現（テスト用の簡易版。真偽値は `t`／`f`）。
fn scalar_text(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Null => None,
        Cell::Integer(v) => Some(v.to_string()),
        Cell::SignedInteger(v) => Some(v.to_string()),
        Cell::Text(s) => Some(s.clone()),
        Cell::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
        other => panic!("unexpected scalar cell {other:?}"),
    }
}

/// セルを文字列へ整形する。経路は `record_out`／`array_out` の引用規則で整形する。
fn text(cell: &Cell) -> String {
    match cell {
        Cell::Record(fields) => record_out(
            &fields
                .iter()
                .map(scalar_text)
                .collect::<Vec<Option<String>>>(),
        ),
        Cell::RecordArray(items) => record_array_out(
            &items
                .iter()
                .map(|f| record_out(&f.iter().map(scalar_text).collect::<Vec<Option<String>>>()))
                .collect::<Vec<String>>(),
        ),
        other => scalar_text(other).unwrap_or_else(|| "NULL".to_string()),
    }
}

fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    run(core, tenant, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
        .iter()
        .map(|r| r.cells.iter().map(text).collect())
        .collect()
}

fn strs(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_string()).collect()
}

/// アンカー 1 から辺をたどる JOIN 形の再帰項（`{tail}` に `CYCLE` 句以降を続ける）。
fn graph_cte(tail: &str) -> String {
    format!(
        "WITH RECURSIVE t(n) AS (SELECT CAST(1 AS BIGINT) UNION ALL \
         SELECT e.dst FROM edges e JOIN t ON e.src = t.n) {tail}"
    )
}

/// 閉路を含むグラフ: 循環した行は印が真で出力され、その行からは再帰しない（そこで停止する）。
#[test]
fn cycle_row_is_marked_and_stops_recursion() {
    let (core, _g) = open_core_with(seed_cycle);
    let sql =
        graph_cte("CYCLE n SET is_cycle USING path SELECT n, is_cycle, path FROM t ORDER BY path");
    assert_eq!(
        rows(&core, "alice", &sql),
        vec![
            strs(&["1", "f", "{(1)}"]),
            strs(&["2", "f", "{(1),(2)}"]),
            strs(&["3", "f", "{(1),(2),(3)}"]),
            strs(&["1", "t", "{(1),(2),(3),(1)}"]),
            strs(&["4", "f", "{(1),(2),(3),(4)}"]),
        ]
    );
}

/// 受入基準: `UNION ALL` は `CYCLE` なしでは反復上限で拒否され、`CYCLE` 句つきでは停止する。
#[test]
fn union_all_terminates_only_with_cycle_clause() {
    let (core, _g) = open_core_with(seed_cycle);
    let plain = graph_cte("SELECT n FROM t");
    assert_eq!(code(&core, "alice", &plain), "54000");
    let with_cycle = graph_cte("CYCLE n SET c USING p SELECT n FROM t");
    assert_eq!(rows(&core, "alice", &with_cycle).len(), 5);
}

/// 主クエリで印を使って循環行を除く。
#[test]
fn main_query_can_filter_on_the_mark() {
    let (core, _g) = open_core_with(seed_cycle);
    let sql = graph_cte(
        "CYCLE n SET is_cycle USING path SELECT n, path FROM t WHERE is_cycle = false ORDER BY path",
    );
    assert_eq!(
        rows(&core, "alice", &sql),
        vec![
            strs(&["1", "{(1)}"]),
            strs(&["2", "{(1),(2)}"]),
            strs(&["3", "{(1),(2),(3)}"]),
            strs(&["4", "{(1),(2),(3),(4)}"]),
        ]
    );
}

/// `SELECT *` の列順は ユーザー列 → 印 → 経路。Describe と Execute の列は一致し、型は boolean／`record[]`。
#[test]
fn star_projection_appends_mark_and_path_and_describe_matches() {
    let (core, _g) = open_core_with(seed_cycle);
    let sql = graph_cte("CYCLE n SET is_cycle USING path SELECT * FROM t");
    let executed = run(&core, "alice", &sql).expect("execute");
    assert_eq!(executed.columns.len(), 3);
    match (&executed.columns[1], &executed.columns[2]) {
        (
            ColumnMeta::Computed {
                name: mark,
                ty: Some(ColumnType::Boolean),
            },
            ColumnMeta::Record {
                name: path,
                fields,
                array: true,
            },
        ) => {
            assert_eq!(mark, "is_cycle");
            assert_eq!(path, "path");
            assert_eq!(fields.len(), 1);
        }
        other => panic!("unexpected mark/path columns: {other:?}"),
    }
    let session = SessionState::default();
    let parsed = core.parse_sql(&sql).expect("parse");
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe")
        .expect("has columns");
    assert_eq!(described, executed.columns);
}

/// 循環判定列が 2 列: 経路の要素は `ROW(a, b)`（引用が必要なので `"(1,2)"` 形）。
#[test]
fn two_cycle_columns_use_row_values() {
    let (core, _g) = open_core_with(seed_cycle);
    let sql = "WITH RECURSIVE t(a, b) AS (SELECT src, dst FROM edges WHERE src = 1 UNION ALL \
               SELECT e.src, e.dst FROM edges e JOIN t ON e.src = t.b) \
               CYCLE a, b SET c USING p SELECT a, b, c, p FROM t ORDER BY p";
    assert_eq!(
        rows(&core, "alice", sql),
        vec![
            strs(&["1", "2", "f", "{\"(1,2)\"}"]),
            strs(&["2", "3", "f", "{\"(1,2)\",\"(2,3)\"}"]),
            strs(&["3", "1", "f", "{\"(1,2)\",\"(2,3)\",\"(3,1)\"}"]),
            strs(&["1", "2", "t", "{\"(1,2)\",\"(2,3)\",\"(3,1)\",\"(1,2)\"}"]),
            strs(&["3", "4", "f", "{\"(1,2)\",\"(2,3)\",\"(3,4)\"}"]),
        ]
    );
}

/// `TO`／`DEFAULT` の型（text・integer・boolean）。
#[test]
fn to_and_default_values_pick_the_common_type() {
    let (core, _g) = open_core_with(seed_cycle);
    for (clause, yes, no) in [
        ("TO 'Y' DEFAULT 'N'", "Y", "N"),
        ("TO 1 DEFAULT 0", "1", "0"),
        ("TO true DEFAULT 'n'", "t", "f"),
        ("TO 'yes' DEFAULT false", "t", "f"),
        ("TO '7' DEFAULT 5", "7", "5"),
        // 引用付き `"true"` は字句段が引用なしへ戻すため真偽値リテラルとして受理される（PG 差分。設計記録参照）。
        ("TO \"true\" DEFAULT false", "t", "f"),
    ] {
        let sql = graph_cte(&format!(
            "CYCLE n SET c {clause} USING p SELECT n, c FROM t ORDER BY p"
        ));
        let got = rows(&core, "alice", &sql);
        assert_eq!(got.len(), 5, "{clause}");
        let marks: Vec<&str> = got.iter().map(|r| r[1].as_str()).collect();
        assert_eq!(marks, vec![no, no, no, yes, no], "{clause}");
    }
}

/// `TO` と `DEFAULT` が同じ値なら、循環でなくても印が `TO` 値と等しく、どの行も再帰しない（アンカーだけ）。
#[test]
fn equal_to_and_default_stops_everything() {
    let (core, _g) = open_core_with(seed_cycle);
    let sql = graph_cte("CYCLE n SET c TO true DEFAULT true USING p SELECT n, c FROM t");
    assert_eq!(rows(&core, "alice", &sql), vec![strs(&["1", "t"])]);
}

/// 型の解決エラー: boolean と integer は `42804`・解釈できない文字列は `22P02`・未対応の定数形は `0A000`。
#[test]
fn mark_value_errors() {
    let (core, _g) = open_core_with(seed_cycle);
    for (clause, want) in [
        ("TO true DEFAULT 1", "42804"),
        ("TO 1 DEFAULT true", "42804"),
        ("TO 1 DEFAULT 'x'", "22P02"),
        ("TO true DEFAULT 'maybe'", "22P02"),
        ("TO 1.5 DEFAULT 0", "0A000"),
        ("TO NULL DEFAULT 0", "0A000"),
        ("TO 1 DEFAULT NULL", "0A000"),
        ("TO date '2020-01-01' DEFAULT 0", "0A000"),
        ("TO -1 DEFAULT 0", "42601"),
        ("TO foo DEFAULT 0", "42601"),
        // 引用付き `"null"` は字句段が引用なしへ戻すため NULL 定数として未対応扱い（`0A000`）。
        ("TO \"null\" DEFAULT false", "0A000"),
        ("TO 1", "42601"),
    ] {
        let sql = graph_cte(&format!("CYCLE n SET c {clause} USING p SELECT n FROM t"));
        let got = run(&core, "alice", &sql)
            .err()
            .map(|e| e.wire_code().to_string());
        assert_eq!(got.as_deref(), Some(want), "{clause}");
    }
}

/// `SEARCH` と `CYCLE` の併用: 列順は `..., 順序列, 印, 経路`。深さ優先の順序で並べられる。
#[test]
fn search_and_cycle_together() {
    let (core, _g) = open_core_with(seed_cycle);
    let sql = graph_cte(
        "SEARCH DEPTH FIRST BY n SET o CYCLE n SET c USING p \
         SELECT n, o, c, p FROM t ORDER BY o",
    );
    assert_eq!(
        rows(&core, "alice", &sql),
        vec![
            strs(&["1", "{(1)}", "f", "{(1)}"]),
            strs(&["2", "{(1),(2)}", "f", "{(1),(2)}"]),
            strs(&["3", "{(1),(2),(3)}", "f", "{(1),(2),(3)}"]),
            strs(&["1", "{(1),(2),(3),(1)}", "t", "{(1),(2),(3),(1)}"]),
            strs(&["4", "{(1),(2),(3),(4)}", "f", "{(1),(2),(3),(4)}"]),
        ]
    );
    let star = graph_cte("SEARCH DEPTH FIRST BY n SET o CYCLE n SET c USING p SELECT * FROM t");
    let result = run(&core, "alice", &star).expect("execute");
    assert_eq!(result.columns.len(), 4);
}

/// 循環判定列に NULL があると `=` が真にならず、循環として検出されない（本実装は反復上限で拒否する）。
#[test]
fn null_fields_never_match() {
    let (core, _g) = open_core_with(seed_cycle);
    let sql =
        "WITH RECURSIVE t(n, x) AS (SELECT CAST(1 AS BIGINT), CAST(NULL AS BIGINT) UNION ALL \
               SELECT e.dst, t.x FROM edges e JOIN t ON e.src = t.n) \
               CYCLE n, x SET c USING p SELECT n FROM t";
    assert_eq!(code(&core, "alice", sql), "54000");
    // 比較対象を n だけにすれば停止する。
    let ok = "WITH RECURSIVE t(n, x) AS (SELECT CAST(1 AS BIGINT), CAST(NULL AS BIGINT) UNION ALL \
              SELECT e.dst, t.x FROM edges e JOIN t ON e.src = t.n) \
              CYCLE n SET c USING p SELECT n FROM t";
    assert_eq!(rows(&core, "alice", ok).len(), 5);
}

/// 単一関係の再帰項（Scan 形）。
#[test]
fn scan_form_recursion() {
    let (core, _g) = open_core_with(|_| {});
    let chain = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
                 CYCLE n SET c USING p SELECT n, c, p FROM t ORDER BY p";
    assert_eq!(
        rows(&core, "alice", chain),
        vec![
            strs(&["1", "f", "{(1)}"]),
            strs(&["2", "f", "{(1),(2)}"]),
            strs(&["3", "f", "{(1),(2),(3)}"]),
        ]
    );
    // 同じ値を返し続ける再帰項は 1 回で循環する。
    let same = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n FROM t) \
                CYCLE n SET c USING p SELECT n, c, p FROM t ORDER BY p";
    assert_eq!(
        rows(&core, "alice", same),
        vec![strs(&["1", "f", "{(1)}"]), strs(&["1", "t", "{(1),(1)}"]),]
    );
}

fn core_with_recursive_limits(
    tag: &str,
    cfg: engine::recursive_cte_limits::RecursiveCteLimits,
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(engine::result_limits::ResultLimits::new(5_000))
        .with_recursive_cte_limits(cfg);
    (core, guard)
}

/// 反復回数・累積行数の上限は `CYCLE` 句つきでも適用される（超過は部分結果なしの `54000`）。
#[test]
fn configured_limits_apply_with_cycle() {
    let chain = |upper: u32| {
        format!(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < {upper}) \
             CYCLE n SET c USING p SELECT count(*) FROM t"
        )
    };
    let (core, _g) = core_with_recursive_limits(
        "sql35-cycle-iter",
        engine::recursive_cte_limits::RecursiveCteLimits::new(10, 100_000),
    );
    assert_eq!(rows(&core, "alice", &chain(10)), vec![strs(&["10"])]);
    assert_eq!(code(&core, "alice", &chain(11)), "54000");
    let (core, _g) = core_with_recursive_limits(
        "sql35-cycle-rows",
        engine::recursive_cte_limits::RecursiveCteLimits::new(1_000, 50),
    );
    assert_eq!(rows(&core, "alice", &chain(50)), vec![strs(&["50"])]);
    assert_eq!(code(&core, "alice", &chain(51)), "54000");
}

/// エラーの SQLSTATE。
#[test]
fn clause_errors() {
    let (core, _g) = open_core_with(|s| {
        seed_cycle(s);
        put_node(s, "alice", 1, 1, None, "root", Visibility::Public);
    });
    let cases: Vec<(String, &str)> = vec![
        // 循環列が列名リストに無い。
        (graph_cte("CYCLE zz SET c USING p SELECT n FROM t"), "42601"),
        // 循環列の重複。
        (
            graph_cte("CYCLE n, n SET c USING p SELECT n FROM t"),
            "42701",
        ),
        // 印・経路の名前が列・相互・SEARCH の SET 名と衝突。
        (graph_cte("CYCLE n SET n USING p SELECT n FROM t"), "42601"),
        (graph_cte("CYCLE n SET c USING n SELECT n FROM t"), "42601"),
        (graph_cte("CYCLE n SET c USING c SELECT n FROM t"), "42601"),
        (
            graph_cte("SEARCH DEPTH FIRST BY n SET o CYCLE n SET o USING p SELECT n FROM t"),
            "42601",
        ),
        (
            graph_cte("SEARCH DEPTH FIRST BY n SET o CYCLE n SET c USING o SELECT n FROM t"),
            "42601",
        ),
        // USING が無い。
        (graph_cte("CYCLE n SET c SELECT n FROM t"), "42601"),
        // 自己参照が無い定義。
        (
            "WITH RECURSIVE t(n) AS (SELECT 1) CYCLE n SET c USING p SELECT n FROM t".to_string(),
            "42601",
        ),
        // 再帰構造の違反（42P19）が CYCLE の検査より先。
        (
            "WITH RECURSIVE t(n) AS (SELECT n FROM t) CYCLE n SET c USING p SELECT n FROM t"
                .to_string(),
            "42P19",
        ),
        // UNION（重複除去）との併用は未対応。
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT n + 1 FROM t WHERE n < 3) \
             CYCLE n SET c USING p SELECT n FROM t"
                .to_string(),
            "0A000",
        ),
        // UNION の左辺が集合演算。
        (
            "WITH RECURSIVE t(node_id) AS (SELECT node_id FROM tree WHERE node_id = 1 \
               UNION SELECT node_id FROM tree WHERE node_id = 8 \
               UNION ALL SELECT c.node_id FROM tree c JOIN t ON c.parent_id = t.node_id) \
             CYCLE node_id SET c USING p SELECT node_id FROM t"
                .to_string(),
            "0A000",
        ),
        // 比較できない型（JSON）の循環列。
        (
            "WITH RECURSIVE t(n, j) AS (SELECT 1, CAST('{}' AS JSON) UNION ALL \
               SELECT n + 1, j FROM t WHERE n < 3) \
             CYCLE j SET c USING p SELECT n FROM t"
                .to_string(),
            "0A000",
        ),
        // 印・経路は再帰項からは見えない（未知列）。
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE c IS NULL) \
             CYCLE n SET c USING p SELECT n FROM t"
                .to_string(),
            "42703",
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(code(&core, "alice", &sql), want, "{sql}");
    }
}

/// RLS-10 (b): 他テナントの行は、印・経路・行数・上限超過の有無のどれにも影響しない。
/// 逆に、可視行だけで閉じる循環は検出される。
#[test]
fn rls_foreign_rows_do_not_affect_cycle() {
    let (core, _g) = open_core_with(|s| {
        // alice の可視辺: 1 → 2（非循環）。bob の private 辺 2 → 1 は alice には見えない。
        put_edge_as(s, "alice", 1, 1, 2, Visibility::Public);
        put_edge_as(s, "bob", 2, 2, 1, Visibility::Private);
    });
    let sql = graph_cte("CYCLE n SET c USING p SELECT n, c, p FROM t ORDER BY p");
    let expected = vec![strs(&["1", "f", "{(1)}"]), strs(&["2", "f", "{(1),(2)}"])];
    assert_eq!(rows(&core, "alice", &sql), expected);
    // 可視行が同じ別テナントからも同じ結果。
    assert_eq!(rows(&core, "carol", &sql), expected);

    let (core, _g) = open_core_with(|s| {
        put_edge_as(s, "alice", 1, 1, 2, Visibility::Public);
        put_edge_as(s, "alice", 2, 2, 1, Visibility::Public);
    });
    assert_eq!(
        rows(&core, "alice", &sql),
        vec![
            strs(&["1", "f", "{(1)}"]),
            strs(&["2", "f", "{(1),(2)}"]),
            strs(&["1", "t", "{(1),(2),(1)}"]),
        ]
    );
}
