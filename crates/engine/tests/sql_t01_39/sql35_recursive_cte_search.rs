//! `WITH RECURSIVE` の `SEARCH DEPTH／BREADTH FIRST` 句の結合テスト（SQL-35、Issue #2213）。
//! ポインタ: `docs/spec/04-behavior/` の SQL-35・RLS-10 (b)。
//!
//! `sql35_recursive_cte_eval.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）。
//! 期待値は PostgreSQL 17 の書き換え意味（深さ優先は経路配列 `record[]`、幅優先は
//! `(深さ, BY 列の値...)` の `record`）から手計算で固定する。順序列は文字列 `{(1),(2)}`／`(0,1)` へ
//! 整形して比較する（整形は `engine::sql::record_text` の引用規則）。

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

use crate::temp_db;
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
    let path = unique_db_path("sql35-recursive-search");
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

/// 根 1・枝（1 → 2, 3）・孫（2 → 4, 5 / 3 → 6）・曾孫（6 → 7）と、別の木（8 → 9）。
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

/// 有向非巡回グラフ（1 → 2, 1 → 3, 2 → 4, 3 → 4, 4 → 5）。節点 4 へ 2 経路ある。
fn seed_dag(storage: &Storage) {
    for (i, (src, dst)) in [(1, 2), (1, 3), (2, 4), (3, 4), (4, 5)].iter().enumerate() {
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

/// スカラーのセルのテキスト表現（テスト用の簡易版。整数・テキスト・NULL のみ）。
fn scalar_text(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Null => None,
        Cell::Integer(v) => Some(v.to_string()),
        Cell::SignedInteger(v) => Some(v.to_string()),
        Cell::Text(s) => Some(s.clone()),
        other => panic!("unexpected scalar cell {other:?}"),
    }
}

/// セルを文字列へ整形する。順序列は `record_out`／`array_out` の引用規則で整形する。
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

/// 1 列目（`n` など）だけを取り出す。
fn first_col(rows: Vec<Vec<String>>) -> Vec<String> {
    rows.into_iter().map(|mut r| r.remove(0)).collect()
}

fn strs(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_string()).collect()
}

const TREE_CTE: &str = "WITH RECURSIVE t(node_id, name) AS (\
       SELECT node_id, name FROM tree WHERE node_id = 1 \
       UNION ALL \
       SELECT c.node_id, c.name FROM tree c JOIN t ON c.parent_id = t.node_id) ";

/// 木（JOIN 形の再帰項）: 深さ優先は先行順（兄弟は BY 値の順）、順序列は経路配列。
#[test]
fn depth_first_tree_orders_by_path() {
    let (core, _g) = open_core_with(seed_tree);
    let sql = format!(
        "{TREE_CTE}SEARCH DEPTH FIRST BY node_id SET ordercol \
         SELECT node_id, ordercol FROM t ORDER BY ordercol"
    );
    assert_eq!(
        rows(&core, "alice", &sql),
        vec![
            strs(&["1", "{(1)}"]),
            strs(&["2", "{(1),(2)}"]),
            strs(&["4", "{(1),(2),(4)}"]),
            strs(&["5", "{(1),(2),(5)}"]),
            strs(&["3", "{(1),(3)}"]),
            strs(&["6", "{(1),(3),(6)}"]),
            strs(&["7", "{(1),(3),(6),(7)}"]),
        ]
    );
    // DESC は全体を反転する。
    let desc = format!(
        "{TREE_CTE}SEARCH DEPTH FIRST BY node_id SET ordercol \
         SELECT node_id FROM t ORDER BY ordercol DESC"
    );
    assert_eq!(
        first_col(rows(&core, "alice", &desc)),
        strs(&["7", "6", "3", "5", "4", "2", "1"])
    );
}

/// 木: 幅優先は深さ → BY 値の順、順序列は `(深さ, BY 値)`。
#[test]
fn breadth_first_tree_orders_by_depth_then_value() {
    let (core, _g) = open_core_with(seed_tree);
    let sql = format!(
        "{TREE_CTE}SEARCH BREADTH FIRST BY node_id SET ordercol \
         SELECT node_id, ordercol FROM t ORDER BY ordercol"
    );
    assert_eq!(
        rows(&core, "alice", &sql),
        vec![
            strs(&["1", "(0,1)"]),
            strs(&["2", "(1,2)"]),
            strs(&["3", "(1,3)"]),
            strs(&["4", "(2,4)"]),
            strs(&["5", "(2,5)"]),
            strs(&["6", "(2,6)"]),
            strs(&["7", "(3,7)"]),
        ]
    );
    let desc = format!(
        "{TREE_CTE}SEARCH BREADTH FIRST BY node_id SET ordercol \
         SELECT node_id FROM t ORDER BY ordercol DESC"
    );
    assert_eq!(
        first_col(rows(&core, "alice", &desc)),
        strs(&["7", "6", "5", "4", "3", "2", "1"])
    );
}

/// DAG: 同じ節点へ 2 経路あると、深さ優先は経路ごとに別行・幅優先は（深さ, 値）が同値の行が並ぶ。
#[test]
fn dag_depth_and_breadth_orders() {
    let (core, _g) = open_core_with(seed_dag);
    let base = "WITH RECURSIVE g(n) AS (SELECT CAST(1 AS BIGINT) UNION ALL \
                SELECT e.dst FROM edges e JOIN g ON e.src = g.n) ";
    let depth = format!("{base}SEARCH DEPTH FIRST BY n SET o SELECT n, o FROM g ORDER BY o");
    assert_eq!(
        rows(&core, "alice", &depth),
        vec![
            strs(&["1", "{(1)}"]),
            strs(&["2", "{(1),(2)}"]),
            strs(&["4", "{(1),(2),(4)}"]),
            strs(&["5", "{(1),(2),(4),(5)}"]),
            strs(&["3", "{(1),(3)}"]),
            strs(&["4", "{(1),(3),(4)}"]),
            strs(&["5", "{(1),(3),(4),(5)}"]),
        ]
    );
    let breadth = format!("{base}SEARCH BREADTH FIRST BY n SET o SELECT n, o FROM g ORDER BY o");
    assert_eq!(
        rows(&core, "alice", &breadth),
        vec![
            strs(&["1", "(0,1)"]),
            strs(&["2", "(1,2)"]),
            strs(&["3", "(1,3)"]),
            strs(&["4", "(2,4)"]),
            strs(&["4", "(2,4)"]),
            strs(&["5", "(3,5)"]),
            strs(&["5", "(3,5)"]),
        ]
    );
}

/// 単一関係の再帰項（`n + 1`）。列名リスト省略（アンカーの別名）でも同じ。
#[test]
fn scan_form_recursion_with_and_without_column_list() {
    let (core, _g) = open_core_with(|_| {});
    let with_list = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 4) \
                     SEARCH DEPTH FIRST BY n SET o SELECT n, o FROM t ORDER BY o DESC";
    assert_eq!(
        rows(&core, "alice", with_list),
        vec![
            strs(&["4", "{(1),(2),(3),(4)}"]),
            strs(&["3", "{(1),(2),(3)}"]),
            strs(&["2", "{(1),(2)}"]),
            strs(&["1", "{(1)}"]),
        ]
    );
    let without_list =
        "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
                        SEARCH BREADTH FIRST BY n SET o SELECT n, o FROM t ORDER BY o";
    assert_eq!(
        rows(&core, "alice", without_list),
        vec![
            strs(&["1", "(0,1)"]),
            strs(&["2", "(1,2)"]),
            strs(&["3", "(2,3)"]),
        ]
    );
}

/// `SELECT *` の末尾に順序列が出る。Describe と Execute の列は一致し、型は `record`／`record[]`。
#[test]
fn star_projection_appends_sequence_column_and_describe_matches() {
    let (core, _g) = open_core_with(seed_tree);
    for (clause, array) in [("DEPTH", true), ("BREADTH", false)] {
        let sql = format!(
            "{TREE_CTE}SEARCH {clause} FIRST BY node_id, name SET ordercol SELECT * FROM t"
        );
        let executed = run(&core, "alice", &sql).expect("execute");
        let names: Vec<String> = executed
            .columns
            .iter()
            .map(|c| match c {
                ColumnMeta::Scalar { name, .. }
                | ColumnMeta::Computed { name, .. }
                | ColumnMeta::AliasedId { name }
                | ColumnMeta::Record { name, .. } => name.clone(),
                ColumnMeta::Id => "id".to_string(),
            })
            .collect();
        assert_eq!(names, strs(&["node_id", "name", "ordercol"]));
        match executed.columns.last() {
            Some(ColumnMeta::Record {
                fields, array: a, ..
            }) => {
                assert_eq!(*a, array);
                // 幅優先は先頭に深さ（BIGINT）が付く。
                assert_eq!(fields.len(), if array { 2 } else { 3 });
            }
            other => panic!("last column must be the sequence column: {other:?}"),
        }
        let session = SessionState::default();
        let parsed = core.parse_sql(&sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        assert_eq!(described, executed.columns);
    }
}

/// BY 2 列（テキスト列を含む）: 引用が必要な値は `record_out`／`array_out` の規則で引用される。
#[test]
fn two_by_columns_use_quoting_rules() {
    let (core, _g) = open_core_with(seed_tree);
    let sql = format!(
        "{TREE_CTE}SEARCH DEPTH FIRST BY node_id, name SET o \
         SELECT node_id, o FROM t ORDER BY o"
    );
    let got = rows(&core, "alice", &sql);
    assert_eq!(
        got.first().cloned(),
        Some(strs(&["1", "{\"(1,root)\"}"])),
        "{got:?}"
    );
    assert_eq!(
        got.get(1).cloned(),
        Some(strs(&["2", "{\"(1,root)\",\"(2,a)\"}"]))
    );
}

/// BY 値に NULL を含む: `NULL` は非 NULL より後（昇順）・DESC は全体の反転で先頭になる。
#[test]
fn null_by_values_sort_last_ascending() {
    let (core, _g) = open_core_with(seed_tree);
    // 再帰項が 1 行も生まない（`name = 'no-such-name'`）ので、結果はアンカーの全行（seq = `(0, parent_id)`）。
    let base = "WITH RECURSIVE t(node_id, parent_id) AS (\
                  SELECT node_id, parent_id FROM tree \
                  UNION ALL \
                  SELECT c.node_id, c.parent_id FROM tree c JOIN t ON c.parent_id = t.node_id \
                  WHERE c.name = 'no-such-name') \
                SEARCH BREADTH FIRST BY parent_id SET o ";
    let asc = format!("{base}SELECT node_id FROM t ORDER BY o, node_id");
    assert_eq!(
        first_col(rows(&core, "alice", &asc)),
        strs(&["2", "3", "4", "5", "6", "7", "9", "1", "8"])
    );
    let desc = format!("{base}SELECT node_id FROM t ORDER BY o DESC, node_id");
    assert_eq!(
        first_col(rows(&core, "alice", &desc)),
        strs(&["1", "8", "9", "7", "6", "4", "5", "2", "3"])
    );
    // NULL を含む順序列のテキスト表現は空フィールド。
    let one = format!("{base}SELECT o FROM t WHERE node_id = 1");
    assert_eq!(
        rows(&core, "alice", &one),
        vec![strs(&["(0,)"])],
        "NULL field is rendered empty"
    );
}

/// `id` 列を BY にした場合（`Cell::Integer(u64)` のままフィールドへ入る）。
#[test]
fn by_id_column() {
    let (core, _g) = open_core_with(seed_tree);
    let sql = "WITH RECURSIVE t(i, n) AS (\
                 SELECT id, node_id FROM tree WHERE node_id = 1 \
                 UNION ALL \
                 SELECT c.id, c.node_id FROM tree c JOIN t ON c.parent_id = t.n) \
               SEARCH BREADTH FIRST BY i SET o SELECT n, o FROM t ORDER BY o";
    let got = rows(&core, "alice", sql);
    assert_eq!(got.len(), 7, "{got:?}");
    assert_eq!(
        got.first().and_then(|r| r.first()).cloned(),
        Some("1".to_string())
    );
}

/// 名前のエラー（PostgreSQL 17 の SQLSTATE）と、評価できない形の `0A000`。
#[test]
fn clause_errors_and_gates() {
    let (core, _g) = open_core_with(seed_tree);
    let anchor_rec = "SELECT node_id, name FROM tree WHERE node_id = 1 UNION ALL \
         SELECT c.node_id, c.name FROM tree c JOIN t ON c.parent_id = t.node_id";
    let cases: &[(String, &str)] = &[
        // BY 列が CTE の列に無い。
        (
            format!("WITH RECURSIVE t(node_id, name) AS ({anchor_rec}) SEARCH DEPTH FIRST BY nope SET o SELECT node_id FROM t"),
            "42601",
        ),
        // BY 列の重複。
        (
            format!("WITH RECURSIVE t(node_id, name) AS ({anchor_rec}) SEARCH DEPTH FIRST BY node_id, node_id SET o SELECT node_id FROM t"),
            "42701",
        ),
        // SET 名が CTE の列名と衝突。
        (
            format!("WITH RECURSIVE t(node_id, name) AS ({anchor_rec}) SEARCH DEPTH FIRST BY node_id SET name SELECT node_id FROM t"),
            "42601",
        ),
        // 自己参照の無い定義への SEARCH。
        (
            "WITH RECURSIVE t(n) AS (SELECT 1) SEARCH DEPTH FIRST BY n SET o SELECT n FROM t".to_string(),
            "42601",
        ),
        // 再帰構造の違反（42P19）が SEARCH の検査より先。
        (
            "WITH RECURSIVE t(n) AS (SELECT n FROM t) SEARCH DEPTH FIRST BY n SET o SELECT n FROM t".to_string(),
            "42P19",
        ),
        // UNION の左辺が集合演算（SEARCH 付きは 0A000）。
        (
            "WITH RECURSIVE t(node_id) AS (SELECT node_id FROM tree WHERE node_id = 1 \
               UNION SELECT node_id FROM tree WHERE node_id = 8 \
               UNION ALL SELECT c.node_id FROM tree c JOIN t ON c.parent_id = t.node_id) \
             SEARCH DEPTH FIRST BY node_id SET o SELECT node_id FROM t"
                .to_string(),
            "0A000",
        ),
        // CYCLE 句は #2214 まで 0A000（SEARCH との併用も同じ）。
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
             SEARCH DEPTH FIRST BY n SET o CYCLE n SET c USING p SELECT n FROM t"
                .to_string(),
            "0A000",
        ),
        // 並べ替えできない型（ベクトル等）の BY 列は計画時に 0A000。
        (
            "WITH RECURSIVE t(n, j) AS (SELECT 1, CAST('{}' AS JSON) UNION ALL \
               SELECT n + 1, j FROM t WHERE n < 3) \
             SEARCH DEPTH FIRST BY j SET o SELECT n FROM t"
                .to_string(),
            "0A000",
        ),
        // 順序列は再帰項の中からは見えない（未知列）。
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE o IS NULL) \
             SEARCH DEPTH FIRST BY n SET o SELECT n FROM t"
                .to_string(),
            "42703",
        ),
        // 順序列を外側の WHERE で参照することは未対応（fail-closed）。
        (
            format!("{TREE_CTE}SEARCH DEPTH FIRST BY node_id SET o SELECT node_id FROM t WHERE o IS NULL"),
            "",
        ),
    ];
    for (sql, want) in cases {
        let err = run(&core, "alice", sql).expect_err(sql);
        if want.is_empty() {
            // 具体的な SQLSTATE は問わず、評価されずに拒否されること（成功しないこと）だけを固定する。
            continue;
        }
        assert_eq!(err.wire_code(), *want, "{sql}: {err:?}");
    }
}

/// SQL 文字列の再連結を行わない: BY 列名・SET 名は識別子としてだけ扱われる（引用符・括弧などを
/// 含む名前は構文エラーで拒否される）。
#[test]
fn malformed_clause_is_syntax_error() {
    let (core, _g) = open_core_with(seed_tree);
    for sql in [
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
         SEARCH DEPTH FIRST BY n SELECT n FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
         SEARCH SIDEWAYS FIRST BY n SET o SELECT n FROM t",
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
         SEARCH DEPTH FIRST BY (n) SET o SELECT n FROM t",
    ] {
        assert_eq!(code(&core, "alice", sql), "42601", "{sql}");
    }
}

/// RLS-10 (b): 他テナントの行は経路・順序・件数のどれにも影響しない（可視行だけで決まる）。
#[test]
fn rls_foreign_rows_do_not_affect_sequence() {
    let (core, _g) = open_core_with(|s| {
        put_node(s, "alice", 1, 1, None, "root", Visibility::Public);
        put_node(s, "alice", 2, 2, Some(1), "a", Visibility::Public);
        put_node(s, "bob", 3, 3, Some(2), "bob-private", Visibility::Private);
        put_node(s, "bob", 4, 4, Some(3), "bob-deeper", Visibility::Private);
    });
    let sql = format!(
        "{TREE_CTE}SEARCH DEPTH FIRST BY node_id SET o SELECT node_id, o FROM t ORDER BY o"
    );
    // alice には bob の Private 行 3・4 は不可視: 経路にも順序にも現れない。
    assert_eq!(
        rows(&core, "alice", &sql),
        vec![strs(&["1", "{(1)}"]), strs(&["2", "{(1),(2)}"])]
    );
    // 可視行が同じ別テナントからも同じ結果（他テナントの存在を観測できない）。
    assert_eq!(
        rows(&core, "carol", &sql),
        vec![strs(&["1", "{(1)}"]), strs(&["2", "{(1),(2)}"])]
    );
}
