//! `WITH RECURSIVE` の反復評価の結合テスト（段階 B。SQL-35、Issue #1870）。
//! ポインタ: `docs/spec/04-behavior/` の SQL-35・SQL-34・SQL-62・RLS-10 (b)・ERR-6。
//!
//! `sql35_recursive_cte_syntax.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）。
//! 期待値は PostgreSQL 17 の結果を手計算で固定する（数列・階層走査・`UNION ALL` の重複保持）。
//! 階層走査の表は `node_id`・`parent_id`・`name` を実列として持つ。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
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

/// 1 ノードを投入する（`parent` が `None` なら根。`row_id` は行 id）。
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

/// 有向辺 `src -> dst` を 1 本投入する。
fn put_edge(storage: &Storage, tenant: &str, row_id: u64, src: i64, dst: i64) {
    engine::tenant::insert_typed_row(
        storage,
        EDGES,
        &ctx(tenant),
        row_id,
        Visibility::Public,
        &[Value::BigInt(src), Value::BigInt(dst)],
        &op_id(&format!("edge-{tenant}-{row_id}")),
    )
    .expect("insert edge");
}

fn open_core_with(seed: impl FnOnce(&Storage)) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql35-recursive-eval");
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

fn open_core() -> (EngineCore, CleanupGuard) {
    open_core_with(|_| {})
}

/// 根 1・2 本の枝（1 → 2, 3）・孫（2 → 4, 5 / 3 → 6）・曾孫（6 → 7）と、別の木（8 → 9）。
fn seed_tree(storage: &Storage) {
    let nodes: [(i64, Option<i64>, &str); 9] = [
        (1, None, "root"),
        (2, Some(1), "a"),
        (3, Some(1), "b"),
        (4, Some(2), "a1"),
        (5, Some(2), "a2"),
        (6, Some(3), "b1"),
        (7, Some(6), "b1x"),
        (8, None, "other"),
        (9, Some(8), "o1"),
    ];
    for (i, (node, parent, name)) in nodes.iter().enumerate() {
        put_node(
            storage,
            "alice",
            (i as u64) + 1,
            *node,
            *parent,
            name,
            Visibility::Public,
        );
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

fn grid(result: &QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(text).collect())
        .collect()
}

fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    grid(&run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}")))
}

fn col(values: &[&str]) -> Vec<Vec<String>> {
    values.iter().map(|v| vec![(*v).to_string()]).collect()
}

fn table(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|v| (*v).to_string()).collect())
        .collect()
}

#[test]
fn sequence_from_const_anchor() {
    let (core, _g) = open_core();
    let sql = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) \
               SELECT n FROM t";
    assert_eq!(
        rows(&core, "alice", sql),
        col(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"])
    );
}

#[test]
fn sequence_aggregate_and_outer_clauses() {
    let (core, _g) = open_core();
    let with = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) ";
    assert_eq!(
        rows(&core, "alice", &format!("{with}SELECT sum(n) FROM t")),
        col(&["55"])
    );
    assert_eq!(
        rows(&core, "alice", &format!("{with}SELECT count(*) FROM t")),
        col(&["10"])
    );
    assert_eq!(
        rows(
            &core,
            "alice",
            &format!("{with}SELECT n FROM t WHERE n > 7 ORDER BY n DESC")
        ),
        col(&["10", "9", "8"])
    );
    assert_eq!(
        rows(&core, "alice", &format!("{with}SELECT n FROM t LIMIT 3")),
        col(&["1", "2", "3"])
    );
}

/// 列名リストを省略した形ではアンカー項の列名（別名）が CTE の列名になる。
#[test]
fn sequence_without_column_list_uses_anchor_names() {
    let (core, _g) = open_core();
    let sql = "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
               SELECT n FROM t";
    assert_eq!(rows(&core, "alice", sql), col(&["1", "2", "3"]));
}

/// 階層走査: 根 1 から木を JOIN で展開する。
#[test]
fn hierarchy_walk_returns_all_descendants() {
    let (core, _g) = open_core_with(seed_tree);
    let sql = "WITH RECURSIVE t(node_id, name) AS (\
                 SELECT node_id, name FROM tree WHERE node_id = 1 \
                 UNION ALL \
                 SELECT c.node_id, c.name FROM tree c JOIN t ON c.parent_id = t.node_id) \
               SELECT node_id, name FROM t ORDER BY node_id";
    assert_eq!(
        rows(&core, "alice", sql),
        table(&[
            &["1", "root"],
            &["2", "a"],
            &["3", "b"],
            &["4", "a1"],
            &["5", "a2"],
            &["6", "b1"],
            &["7", "b1x"],
        ])
    );
}

/// 全根から展開すると、全ノードが 1 度ずつ現れる。
#[test]
fn hierarchy_walk_from_all_roots() {
    let (core, _g) = open_core_with(seed_tree);
    let sql = "WITH RECURSIVE t(node_id) AS (\
                 SELECT node_id FROM tree WHERE parent_id IS NULL \
                 UNION ALL \
                 SELECT c.node_id FROM tree c JOIN t ON c.parent_id = t.node_id) \
               SELECT node_id FROM t ORDER BY node_id";
    assert_eq!(
        rows(&core, "alice", sql),
        col(&["1", "2", "3", "4", "5", "6", "7", "8", "9"])
    );
}

/// 階層走査の結果に対する主クエリの WHERE・集計。
#[test]
fn hierarchy_walk_with_main_clauses() {
    let (core, _g) = open_core_with(seed_tree);
    let with = "WITH RECURSIVE t(node_id, name) AS (\
                  SELECT node_id, name FROM tree WHERE node_id = 1 \
                  UNION ALL \
                  SELECT c.node_id, c.name FROM tree c JOIN t ON c.parent_id = t.node_id) ";
    assert_eq!(
        rows(
            &core,
            "alice",
            &format!("{with}SELECT node_id FROM t WHERE node_id >= 4 ORDER BY node_id")
        ),
        col(&["4", "5", "6", "7"])
    );
    assert_eq!(
        rows(&core, "alice", &format!("{with}SELECT count(*) FROM t")),
        col(&["7"])
    );
}

/// 再帰項の `WHERE` が作業表側の列だけを参照する形（側スキャンへ押し下げられる述語）。
/// 反復 2 では作業表の行 2・3（名前が `root` でない）が除外されるため、その子へは進まない。
#[test]
fn hierarchy_walk_with_working_table_filter() {
    let (core, _g) = open_core_with(seed_tree);
    let sql = "WITH RECURSIVE t(node_id, name) AS (\
                 SELECT node_id, name FROM tree WHERE node_id = 1 \
                 UNION ALL \
                 SELECT c.node_id, c.name FROM tree c JOIN t ON c.parent_id = t.node_id \
                 WHERE t.name = 'root') \
               SELECT node_id FROM t ORDER BY node_id";
    assert_eq!(rows(&core, "alice", sql), col(&["1", "2", "3"]));
}

/// 式項目を持つ複数列の数列（平方数: n² = (n-1)² + 2n - 1）。
#[test]
fn sequence_with_computed_columns() {
    let (core, _g) = open_core();
    let sql = "WITH RECURSIVE t(n, sq) AS (\
                 SELECT 1, 1 UNION ALL SELECT n + 1, sq + 2 * n + 1 FROM t WHERE n < 5) \
               SELECT n, sq FROM t";
    assert_eq!(
        rows(&core, "alice", sql),
        table(&[
            &["1", "1"],
            &["2", "4"],
            &["3", "9"],
            &["4", "16"],
            &["5", "25"]
        ])
    );
}

/// `UNION ALL` は重複を保持する（菱形グラフで節点 4 が 2 回現れる）。
#[test]
fn union_all_keeps_duplicates() {
    let (core, _g) = open_core_with(|s| {
        put_edge(s, "alice", 1, 1, 2);
        put_edge(s, "alice", 2, 1, 3);
        put_edge(s, "alice", 3, 2, 4);
        put_edge(s, "alice", 4, 3, 4);
    });
    let sql = "WITH RECURSIVE r(n) AS (\
                 SELECT CAST(1 AS BIGINT) UNION ALL SELECT e.dst FROM edges e JOIN r ON e.src = r.n) \
               SELECT n FROM r ORDER BY n";
    assert_eq!(rows(&core, "alice", sql), col(&["1", "2", "3", "4", "4"]));
}

#[test]
fn empty_anchor_and_empty_first_iteration() {
    let (core, _g) = open_core_with(seed_tree);
    assert!(rows(
        &core,
        "alice",
        "WITH RECURSIVE t(n) AS (SELECT node_id FROM tree WHERE node_id = 999 \
           UNION ALL SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t"
    )
    .is_empty());
    assert_eq!(
        rows(
            &core,
            "alice",
            "WITH RECURSIVE t(n) AS (SELECT 5 UNION ALL SELECT n + 1 FROM t WHERE n < 0) \
             SELECT n FROM t"
        ),
        col(&["5"])
    );
}

/// 反復回数の上限（暫定の固定値 1000。再帰項の実行回数で数え、0 行になる最後の実行も含む）。
/// `--max-result-rows` を上限回数より大きくして、行数の上限が先に効かないようにする。
#[test]
fn iteration_limit_boundary() {
    let path = unique_db_path("sql35-recursive-iter");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(engine::result_limits::ResultLimits::new(5_000));
    let at_limit =
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 1000) \
                    SELECT count(*) FROM t";
    assert_eq!(rows(&core, "alice", at_limit), col(&["1000"]));
    let over = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 1001) \
                SELECT count(*) FROM t";
    assert_eq!(code(&core, "alice", over), "54000");
}

/// 終了条件の無い再帰は部分結果を返さず `54000` で止まる。
#[test]
fn unbounded_recursion_stops_with_54000() {
    let (core, _g) = open_core();
    let sql = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT n FROM t";
    assert_eq!(code(&core, "alice", sql), "54000");
    let agg = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) \
               SELECT count(*) FROM t";
    assert_eq!(code(&core, "alice", agg), "54000");
}

/// 反復ごとに行が倍増する再帰は、行数の上限で `54000` になる（反復回数の上限より先）。
#[test]
fn row_limit_stops_exploding_recursion() {
    let (core, _g) = open_core_with(|s| {
        put_edge(s, "alice", 1, 1, 1);
        put_edge(s, "alice", 2, 1, 1);
    });
    let sql = "WITH RECURSIVE r(n) AS (\
                 SELECT CAST(1 AS BIGINT) UNION ALL SELECT e.dst FROM edges e JOIN r ON e.src = r.n) \
               SELECT count(*) FROM r";
    assert_eq!(code(&core, "alice", sql), "54000");
}

/// 反復ごとに中断要求へ協調する（`57014`）。上限（`54000`）より先に中断が効く。
#[test]
fn iteration_cooperates_with_interrupt() {
    let (core, _g) = open_core();
    let _scope = engine::interrupt::InterruptScope::install(Box::new(|| {
        Some(engine::interrupt::InterruptReason::ClientGone)
    }));
    let sql = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT n FROM t";
    assert_eq!(code(&core, "alice", sql), "57014");
}

/// 最終結果が `--max-result-rows` を超えたら `54000`（SQL-34）。
#[test]
fn result_row_cap_applies() {
    let path = unique_db_path("sql35-recursive-cap");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(engine::result_limits::ResultLimits::new(5));
    let ok = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 5) \
              SELECT n FROM t";
    assert_eq!(rows(&core, "alice", ok).len(), 5);
    let over = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 6) \
                SELECT n FROM t";
    assert_eq!(code(&core, "alice", over), "54000");
}

#[test]
fn column_mismatches_use_set_operation_codes() {
    let (core, _g) = open_core();
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1, 2 FROM t WHERE n < 3) \
             SELECT n FROM t"
        ),
        "42804"
    );
    // 型不一致（アンカー項は整数、再帰項はテキスト）。
    let (core, _g2) = open_core_with(seed_tree);
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL \
               SELECT c.name FROM tree c JOIN t ON c.parent_id = t.n) \
             SELECT n FROM t"
        ),
        "42804"
    );
    // 列名リストがアンカー項の列数より長い。
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH RECURSIVE t(a, b) AS (SELECT 1 UNION ALL SELECT a + 1 FROM t WHERE a < 3) \
             SELECT a FROM t"
        ),
        "42601"
    );
}

/// アンカー項が疑似列 `id` を返す形（列名リストで改名する）。
#[test]
fn pseudo_id_anchor_column() {
    let (core, _g) = open_core_with(seed_tree);
    let sql = "WITH RECURSIVE t(i) AS (\
                 SELECT id FROM tree WHERE name = 'root' \
                 UNION ALL \
                 SELECT c.id FROM tree c JOIN t ON c.parent_id = t.i) \
               SELECT i FROM t ORDER BY i";
    assert_eq!(
        rows(&core, "alice", sql),
        col(&["1", "2", "3", "4", "5", "6", "7"])
    );
}

/// 作業表は疑似列 `id` を持たないため、`SELECT *`・`t.*` は実列だけへ展開される（`id` を含まない）。
/// 疑似列 `id` の直接投影は黙って 0／NULL にせず拒否する（fail-closed）。
#[test]
fn recursive_reference_star_expands_to_real_columns_only() {
    let (core, _g) = open_core_with(seed_tree);
    // 実列だけへ展開され、終端する（`n` は変化しないため `WHERE` で 1 回だけ通す）。
    let star = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT * FROM t WHERE n < 1) \
                SELECT n FROM t";
    assert_eq!(rows(&core, "alice", star), col(&["1"]));
    let sql =
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT id FROM t WHERE n < 3) SELECT n FROM t";
    let c = code(&core, "alice", sql);
    assert!(["0A000", "42601", "42703"].contains(&c.as_str()), "{c}");
}

/// 作業表は疑似列 `id` を持たない: 述語・式項目・JOIN 条件・外側の参照のどれでも `id` を
/// 黙って 0 として評価せず束縛段で拒否する（Issue #1870 レビュー指摘）。
#[test]
fn recursive_reference_rejects_pseudo_id_in_expressions() {
    let (core, _g) = open_core_with(seed_tree);
    for sql in [
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3 AND id = 0) \
           SELECT n FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE id > 0) SELECT n FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT id + 1 FROM t WHERE n < 3) SELECT n FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL \
           SELECT c.node_id FROM tree c JOIN t ON c.parent_id = t.n WHERE t.id = 0) SELECT n FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL \
           SELECT c.node_id FROM tree c JOIN t ON t.id = c.parent_id) SELECT n FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
           SELECT n FROM t WHERE id = 0",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
           SELECT n FROM t ORDER BY id",
    ] {
        let c = code(&core, "alice", sql);
        assert!(["0A000", "42601", "42703", "22000"].contains(&c.as_str()), "{sql}: {c}");
    }
    // 実列 `id` を持つ作業表では従来どおり参照できる（疑似列ではなく実列として束縛）。
    let ok = "WITH RECURSIVE t(id) AS (SELECT 1 UNION ALL SELECT id + 1 FROM t WHERE id < 3) \
              SELECT id FROM t ORDER BY id";
    assert_eq!(rows(&core, "alice", ok), col(&["1", "2", "3"]));
}

/// ICU 照合の列を持つ表を参照する再帰は、照合を作業表へ運べないため固定の `0A000` で拒否する
/// （バイト列順での黙った比較にしない。TABLE-28）。
#[test]
fn icu_collated_table_is_rejected() {
    let (core, _g) = open_core_with(|storage| {
        let schema = TableSchema::new(
            "words",
            vec![
                ColumnDef::new("w", ColumnType::Text, false)
                    .with_collation(engine::collation::Collation::UndIcu),
                ColumnDef::new("n", ColumnType::BigInt, false),
            ],
        );
        storage
            .create_table(&schema)
            .expect("create collated table");
    });
    let sql = "WITH RECURSIVE t(w, n) AS (\
                 SELECT w, n FROM words WHERE n = 1 \
                 UNION ALL SELECT w, n + 1 FROM t WHERE w < 'm' AND n < 3) \
               SELECT n FROM t";
    let err = run(&core, "alice", sql).expect_err("collated recursion must be rejected");
    assert_eq!(code(&core, "alice", sql), "0A000");
    assert!(
        format!("{err:?}").contains("this collation is not supported in this expression"),
        "{err:?}"
    );
}

/// 評価できない形は `0A000`（`UNION`・複数定義・SEARCH／CYCLE）。段階 A の `42P19` が先に決まる。
#[test]
fn feature_gates_remain() {
    let (core, _g) = open_core();
    for sql in [
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t",
        "WITH RECURSIVE a AS (SELECT 1), t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
         SELECT n FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
         CYCLE n SET c USING p SELECT n FROM t",
    ] {
        assert_eq!(code(&core, "alice", sql), "0A000", "{sql}");
    }
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH RECURSIVE t(n) AS (SELECT n FROM t UNION ALL SELECT 1) SELECT n FROM t"
        ),
        "42P19"
    );
}

/// 自己参照が 1 つも無い `WITH RECURSIVE` は通常の CTE と同じ結果になる。
#[test]
fn non_self_referencing_equals_plain_with() {
    let (core, _g) = open_core_with(seed_tree);
    let recursive = "WITH RECURSIVE a AS (SELECT node_id, name FROM tree WHERE name = 'root') \
                     SELECT node_id, name FROM a ORDER BY node_id";
    let plain = "WITH a AS (SELECT node_id, name FROM tree WHERE name = 'root') \
                 SELECT node_id, name FROM a ORDER BY node_id";
    assert_eq!(rows(&core, "alice", recursive), rows(&core, "alice", plain));
    assert_eq!(rows(&core, "alice", recursive).len(), 1);
}

/// P0: CTE と同名の実表があっても、再帰項の自己参照は実表を読まず作業表だけを読む。
#[test]
fn recursive_reference_shadows_real_table() {
    let (core, _g) = open_core_with(seed_tree);
    let scan_form = "WITH RECURSIVE tree(n) AS \
                       (SELECT 1 UNION ALL SELECT n + 1 FROM tree WHERE n < 3) \
                     SELECT n FROM tree";
    assert_eq!(rows(&core, "alice", scan_form), col(&["1", "2", "3"]));
    // JOIN 形でも同名の実表 `tree`（9 行）は読まれず、作業表 `tree` と辺の結合だけが評価される。
    let (core, _g) = open_core_with(|s| {
        seed_tree(s);
        put_edge(s, "alice", 1, 1, 2);
        put_edge(s, "alice", 2, 2, 3);
    });
    let join_form = "WITH RECURSIVE tree(n) AS (\
                       SELECT CAST(1 AS BIGINT) UNION ALL SELECT e.dst FROM edges e JOIN tree ON e.src = tree.n) \
                     SELECT n FROM tree ORDER BY n";
    assert_eq!(rows(&core, "alice", join_form), col(&["1", "2", "3"]));
}

/// RLS の基本ケース（網羅は #1873）: 他テナントの Private 行を経由する枝は辿られない。
#[test]
fn rls_hides_foreign_private_branch() {
    let (core, _g) = open_core_with(|s| {
        put_node(s, "alice", 1, 1, None, "root", Visibility::Public);
        put_node(
            s,
            "alice",
            2,
            2,
            Some(1),
            "alice-private",
            Visibility::Private,
        );
        put_node(s, "bob", 3, 3, Some(2), "bob-private", Visibility::Private);
        put_node(s, "bob", 4, 4, Some(3), "bob-deeper", Visibility::Private);
    });
    let sql = "WITH RECURSIVE t(node_id) AS (\
                 SELECT node_id FROM tree WHERE node_id = 1 \
                 UNION ALL \
                 SELECT c.node_id FROM tree c JOIN t ON c.parent_id = t.node_id) \
               SELECT node_id FROM t ORDER BY node_id";
    // alice は自分の Private 行 2 を辿れるが、bob の Private 行 3・4 は不可視。
    assert_eq!(rows(&core, "alice", sql), col(&["1", "2"]));
    // bob には alice の Private 行 2 が不可視のため、bob 自身の行 3・4 へ到達できない。
    assert_eq!(rows(&core, "bob", sql), col(&["1"]));
}

/// Describe（拡張クエリ）の列は Execute と一致し、束縛エラーも一致する。
#[test]
fn describe_matches_execute() {
    let (core, _g) = open_core_with(seed_tree);
    for sql in [
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) \
         SELECT n FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) \
         SELECT sum(n) FROM t",
        "WITH RECURSIVE t(node_id, name) AS (SELECT node_id, name FROM tree WHERE node_id = 1 \
           UNION ALL SELECT c.node_id, c.name FROM tree c JOIN t ON c.parent_id = t.node_id) \
         SELECT node_id, name FROM t",
    ] {
        let session = SessionState::default();
        let parsed = core.parse_sql(sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        let executed = run(&core, "alice", sql).expect("execute");
        assert_eq!(described, executed.columns, "sql={sql}");
    }
    // 束縛エラー（列数不一致）は Describe でも Execute と同じ SQLSTATE。
    let bad = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1, 2 FROM t WHERE n < 3) \
               SELECT n FROM t";
    let session = SessionState::default();
    let parsed = core.parse_sql(bad).expect("parse");
    let err = core
        .describe_parsed_in_session(&session, &parsed)
        .expect_err("describe must fail");
    assert_eq!(err.wire_code(), "42804");
}

/// Describe の SQLSTATE（`None` は Describe が成功する）。
fn describe_code(core: &EngineCore, sql: &str) -> Option<String> {
    let session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse");
    core.describe_parsed_in_session(&session, &parsed)
        .err()
        .map(|e| e.wire_code().to_string())
}

/// 作業表の型の写像（Issue #1870 レビュー指摘）: 整数型の拡大は `42804`、作業表で評価できない型は
/// 計画時に固定 `0A000`、疑似列 `id` を結果列にしたアンカー項は BIGINT の実列として再帰できる。
/// Execute と Describe が同じ SQLSTATE を返す。
#[test]
fn work_table_type_mapping_is_consistent() {
    let (core, _g) = open_core_with(seed_tree);
    let cases: &[(&str, &str)] = &[
        // 整数の拡大（SMALLINT アンカー＋INTEGER の再帰項）は事前に 42804。
        (
            "WITH RECURSIVE t(n) AS (SELECT CAST(32767 AS SMALLINT) UNION ALL \
               SELECT CAST(n AS INTEGER) + 1 FROM t WHERE n < 32768) SELECT n FROM t",
            "42804",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT CAST(1 AS INTEGER) UNION ALL \
               SELECT CAST(n AS BIGINT) + 1 FROM t WHERE n < 3) SELECT n FROM t",
            "42804",
        ),
        // 型が一致していれば受理（SMALLINT のまま再帰）。
        (
            "WITH RECURSIVE t(n) AS (SELECT CAST(1 AS SMALLINT) UNION ALL \
               SELECT CAST(n + 1 AS SMALLINT) FROM t WHERE n < 3) SELECT n FROM t",
            "",
        ),
        // 作業表から述語・式へ渡せない型は 0A000（実行時 Internal にしない）。
        (
            "WITH RECURSIVE t(n, tm) AS (SELECT 1, CAST('12:00:00' AS TIME) UNION ALL \
               SELECT n + 1, tm FROM t WHERE n < 2) SELECT n FROM t",
            "0A000",
        ),
        (
            "WITH RECURSIVE t(n, tm) AS (SELECT 1, CAST('2024-01-01 00:00:00+00' AS TIMESTAMPTZ) \
               UNION ALL SELECT n + 1, tm FROM t WHERE n < 2) SELECT n FROM t",
            "0A000",
        ),
        (
            "WITH RECURSIVE t(n, iv) AS (SELECT 1, CAST('1 day' AS INTERVAL) UNION ALL \
               SELECT n + 1, iv FROM t WHERE n < 2) SELECT n FROM t",
            "0A000",
        ),
        // 疑似列 `id` のアンカー（BIGINT の実列）。再帰項が実列を返しても、`id` を返しても受理。
        (
            "WITH RECURSIVE t(i) AS (SELECT id FROM tree WHERE name = 'root' UNION ALL \
               SELECT i FROM t WHERE i < 0) SELECT i FROM t",
            "",
        ),
        (
            "WITH RECURSIVE t(i) AS (SELECT id FROM tree WHERE name = 'root' UNION ALL \
               SELECT i + 1 FROM t WHERE i < 0) SELECT i FROM t",
            "",
        ),
        (
            "WITH RECURSIVE t(i) AS (SELECT id FROM tree WHERE name = 'root' UNION ALL \
               SELECT c.id FROM tree c JOIN t ON c.parent_id = t.i) SELECT i FROM t",
            "",
        ),
    ];
    for (sql, expected) in cases {
        let got = match run(&core, "alice", sql) {
            Ok(_) => String::new(),
            Err(e) => e.wire_code().to_string(),
        };
        assert_eq!(&got, expected, "execute: {sql}");
        let described = describe_code(&core, sql).unwrap_or_default();
        assert_eq!(&described, expected, "describe: {sql}");
    }
    // 疑似列 `id` のアンカーの値は作業表を経て保たれる。
    let ids = rows(
        &core,
        "alice",
        "WITH RECURSIVE t(i) AS (SELECT id FROM tree WHERE name = 'root' UNION ALL \
           SELECT i FROM t WHERE i < 0) SELECT count(*) FROM t",
    );
    assert_eq!(ids, col(&["1"]));
}
