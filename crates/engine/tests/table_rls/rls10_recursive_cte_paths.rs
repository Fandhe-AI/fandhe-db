//! 再帰 CTE（`WITH RECURSIVE`）の全反復でテナント境界が保たれることを、機械的なマトリクスで固定する
//! 回帰テスト（Issue #1873。ポインタ: RLS-10 (b)・SQL-35、関連 SQL-46・SQL-34・TABLE-12・ERR-6）。
//!
//! 役割: `sql35_recursive_cte_*.rs` は機能単位で RLS のケースを 1 件ずつ持つ。本ファイルは閲覧テナント
//! 3 つ × 可視性 2 モード × SQL 形（Scan 形・JOIN 形・UNION ALL／UNION・SEARCH・外側集計・辺の循環）で
//! 横断的に検査する。production の可視性判定（`PolicyContext::is_visible` 等）はオラクル側で呼ばない。
//!
//! 「各反復」の読み取り点（engine の再帰評価 `sql/recursive_eval.rs`・`core.rs` の再帰 CTE の腕）:
//! - アンカー項は通常の読み取り経路（閲覧者の `PolicyContext`・同一 `read_txn`）を通る
//! - JOIN 形の再帰項の基底表側は、反復の前に 1 度だけ走査され全反復で使い回される
//! - Scan 形の再帰項と作業表の更新はメモリ上だけで、前回反復の可視行から作った値だけで構成される
//!
//! 他テナントの Private 行が 1 行でも混入すれば、深さごとの行集合・停止点・`54000` の有無のどれかが
//! 変わる。そこで次の軸で確かめる。
//! - T0 受理ゲート: 全形が baseline で成功し、軸ごとに十分な件数がある（空振り防止）
//! - T1 独立オラクル: 結果（行・順序・深さ・経路）がテスト側の真実値からの純 Rust 計算と一致し、
//!   不可視行のトークンが現れない
//! - T2 baseline／flooded の完全差分: 閲覧者から見えない行（他テナントの Private）だけを足しても応答が
//!   完全一致し、他テナントの Public 行しか持たない対照テナントの応答とも一致する
//! - T3 エラー応答: 静的エラーと、データ依存の `54000`（反復回数・累積行数・JOIN 入力行数・結果行数・
//!   UNION ALL の循環）が他テナント行で変わらない（陽性対照つき）。文面に他テナントの情報が無い
//! - T5 キャッシュの温め順序に応答が依存しない
//! - T6 負の対照: 捏造した違反を検査器が検出する

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::recursive_cte_limits::RecursiveCteLimits;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::record_text::{record_array_out, record_out};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TREE: &str = "tree";
const EDGES: &str = "edges";
const ALICE: &str = "alice";
const BOB: &str = "bob";
const CAROL: &str = "carol";

/// 閲覧者 × 可視性モード（`true` は Private も許可）。
const VIEWERS: [(&str, bool); 6] = [
    (ALICE, false),
    (ALICE, true),
    (BOB, false),
    (BOB, true),
    (CAROL, false),
    (CAROL, true),
];

// ---------------------------------------------------------------------------
// fixture（真実値）
// ---------------------------------------------------------------------------

/// 投入した 1 行の真実値。`tree` は `a = node_id`・`b = parent_id`、`edges` は `a = src`・`b = dst`。
#[derive(Clone, Debug)]
struct Truth {
    table: &'static str,
    tenant: &'static str,
    row_id: u64,
    a: i64,
    b: Option<i64>,
    vis: Visibility,
}

impl Truth {
    /// 禁止トークン用の一意なラベル（`tree` のみ。全テキストセルの走査に使う）。
    fn label(&self) -> String {
        format!("uniquetoken_{}_{}_{}", self.table, self.tenant, self.row_id)
    }
}

/// production の `is_visible` を使わない独立の可視判定。
fn allowed(t: &Truth, viewer: &str, allow_private: bool) -> bool {
    match t.vis {
        Visibility::Public => true,
        Visibility::Private => allow_private && t.tenant == viewer,
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

/// baseline（閲覧者から見える行）と、他テナントの Private 行だけを足した flooded の真実値。
fn fixture(flooded: bool) -> Vec<Truth> {
    let mut rows: Vec<Truth> = Vec::new();
    let mut next = [0u64, 0u64];
    let mut put =
        |table: &'static str, tenant: &'static str, a: i64, b: Option<i64>, vis: Visibility| {
            let slot = usize::from(table == EDGES);
            next[slot] += 1;
            rows.push(Truth {
                table,
                tenant,
                row_id: next[slot],
                a,
                b,
                vis,
            });
        };
    use Visibility::{Private, Public};
    // 根 1 から 1→2→3→4→5 の鎖・分岐 1→6→7・alice の Private 葉 8（3 の子）。
    for (n, p) in [
        (1, None),
        (2, Some(1)),
        (3, Some(2)),
        (4, Some(3)),
        (5, Some(4)),
    ] {
        put(TREE, ALICE, n, p, Public);
    }
    put(TREE, ALICE, 6, Some(1), Public);
    put(TREE, ALICE, 7, Some(6), Public);
    put(TREE, ALICE, 8, Some(3), Private);
    // 他テナントの Public 行は全閲覧者に見える。孤児 201（親 200 は flooded でだけ存在する）。
    put(TREE, BOB, 9, Some(2), Public);
    put(TREE, CAROL, 10, Some(7), Public);
    put(TREE, ALICE, 201, Some(200), Public);
    // 辺: 循環 1→2→3→1、鎖 10→11→12、alice の Private 辺 12→13。
    for (s, d) in [(1, 2), (2, 3), (3, 1), (10, 11), (11, 12)] {
        put(EDGES, ALICE, s, Some(d), Public);
    }
    put(EDGES, ALICE, 12, Some(13), Private);
    if flooded {
        // どの深さにも混入しうる位置に、各可視ノードの子。
        for p in 1..=8 {
            put(TREE, BOB, 100 + p, Some(p), Private);
        }
        // 橋渡し: 5 → 200（bob）→ 201（Public の孤児）。
        put(TREE, BOB, 200, Some(5), Private);
        // TABLE-12 の衝突: alice の可視行と同じ (node_id, parent_id)。
        put(TREE, BOB, 3, Some(2), Private);
        put(TREE, BOB, 8, Some(3), Private);
        // 余分な根と、その子。
        put(TREE, BOB, 300, None, Private);
        put(TREE, BOB, 301, Some(300), Private);
        // 長い鎖（反復回数の上限を超える）。
        put(TREE, BOB, 500, Some(1), Private);
        for k in 1..10 {
            put(TREE, BOB, 500 + k, Some(499 + k), Private);
        }
        // 扇状の展開（累積行数・入力行数の上限を超える）。
        for k in 0..30 {
            put(TREE, BOB, 600 + k, Some(1), Private);
        }
        // 辺: 到達範囲の拡張と、UNION ALL の循環を閉じる辺。
        put(EDGES, BOB, 3, Some(4), Private);
        put(EDGES, BOB, 4, Some(5), Private);
        put(EDGES, BOB, 12, Some(10), Private);
    }
    rows
}

fn tree_schema() -> TableSchema {
    TableSchema::new(
        TREE,
        vec![
            ColumnDef::new("node_id", ColumnType::BigInt, false),
            ColumnDef::new("parent_id", ColumnType::BigInt, true),
            ColumnDef::new("label", ColumnType::Text, false),
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

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

/// core の構成（起動時設定の差し替え）。
#[derive(Clone, Copy, Debug)]
enum Cfg {
    /// 既定（反復 40・累積行 5,000）。
    Default,
    /// 反復回数の上限。
    Iter(usize),
    /// 累積行数の上限。
    Rows(usize),
    /// JOIN の入力行数の上限。
    JoinInput(usize),
    /// `--max-result-rows`。
    Result(usize),
}

fn build_core(flooded: bool, cfg: Cfg) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("rls10-recursive-paths");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&tree_schema()).expect("create tree");
    storage.create_table(&edges_schema()).expect("create edges");
    for t in fixture(flooded) {
        let values = if t.table == TREE {
            vec![
                Value::BigInt(t.a),
                t.b.map_or(Value::Null, Value::BigInt),
                Value::Text(t.label()),
            ]
        } else {
            vec![Value::BigInt(t.a), Value::BigInt(t.b.expect("dst"))]
        };
        engine::tenant::insert_typed_row(
            &storage,
            t.table,
            &ctx_for(t.tenant, true),
            t.row_id,
            t.vis,
            &values,
            &OperationId::parse(&format!("op-{}-{}-{}", t.table, t.tenant, t.row_id))
                .expect("operation id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match cfg {
        Cfg::Default => core.with_recursive_cte_limits(RecursiveCteLimits::new(40, 5_000)),
        Cfg::Iter(n) => core.with_recursive_cte_limits(RecursiveCteLimits::new(n, 100_000)),
        Cfg::Rows(n) => core.with_recursive_cte_limits(RecursiveCteLimits::new(40, n)),
        Cfg::JoinInput(n) => core
            .with_recursive_cte_limits(RecursiveCteLimits::new(40, 5_000))
            .with_join_limits(JoinSizeLimits::new(8, n, 100_000)),
        Cfg::Result(n) => core
            .with_recursive_cte_limits(RecursiveCteLimits::new(40, 5_000))
            .with_result_limits(ResultLimits::new(n)),
    };
    (core, guard)
}

// ---------------------------------------------------------------------------
// 実行ヘルパ
// ---------------------------------------------------------------------------

/// 応答。成功は行の文字列表現、失敗は `(wire_code, client_message)`。
type Resp = Result<Vec<Vec<String>>, (String, String)>;

fn scalar_text(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Null => None,
        Cell::Integer(v) => Some(v.to_string()),
        Cell::SignedInteger(v) => Some(v.to_string()),
        Cell::Float(v) => Some(v.to_string()),
        Cell::Text(s) => Some(s.clone()),
        // `sum(bigint)` は scale 0 の numeric。整数と同じ表記で比べる。
        Cell::Numeric(d) if d.scale() == 0 => Some(d.unscaled().to_string()),
        other => panic!("unexpected scalar cell {other:?}"),
    }
}

/// セルの文字列表現。SEARCH の順序列（レコード・レコード配列）は engine の出力規則で整形する。
fn text(cell: &Cell) -> String {
    match cell {
        Cell::Record(fields) => record_out(&fields.iter().map(scalar_text).collect::<Vec<_>>()),
        Cell::RecordArray(items) => record_array_out(
            &items
                .iter()
                .map(|f| record_out(&f.iter().map(scalar_text).collect::<Vec<_>>()))
                .collect::<Vec<String>>(),
        ),
        other => scalar_text(other).unwrap_or_else(|| "NULL".to_string()),
    }
}

fn run(core: &EngineCore, viewer: &str, allow_private: bool, sql: &str) -> Resp {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx_for(viewer, allow_private), &mut session, sql) {
        Ok(SqlOutcome::Query(result)) => Ok(result
            .rows
            .iter()
            .map(|r| r.cells.iter().map(text).collect())
            .collect()),
        Ok(other) => panic!("expected Query outcome for {sql}, got {other:?}"),
        Err(e) => Err((e.wire_code().to_string(), e.client_message())),
    }
}

/// 閲覧者に見えない `tree` 行のラベル（禁止トークン）。
fn forbidden_labels(truth: &[Truth], viewer: &str, allow_private: bool) -> Vec<String> {
    truth
        .iter()
        .filter(|t| t.table == TREE && !allowed(t, viewer, allow_private))
        .map(Truth::label)
        .collect()
}

/// 全テキストセル・エラー文面に、不可視行のトークンと他テナント名が無いことを確かめる。
fn assert_no_leak(resp: &Resp, forbidden: &[String], foreign_tenants: &[&str], ctx_msg: &str) {
    let mut haystack: Vec<&str> = Vec::new();
    match resp {
        Ok(rows) => rows.iter().flatten().for_each(|c| haystack.push(c)),
        Err((code, msg)) => {
            haystack.push(code);
            haystack.push(msg);
            for tenant in foreign_tenants {
                assert!(
                    !msg.contains(tenant),
                    "{ctx_msg}: tenant name in error: {msg}"
                );
            }
        }
    }
    for cell in haystack {
        for token in forbidden {
            assert!(
                !cell.contains(token.as_str()),
                "{ctx_msg}: leaked {token} in {cell}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 独立オラクル（純 Rust）
// ---------------------------------------------------------------------------

fn visible<'a>(
    truth: &'a [Truth],
    table: &str,
    viewer: &str,
    allow_private: bool,
) -> Vec<&'a Truth> {
    truth
        .iter()
        .filter(|t| t.table == table && allowed(t, viewer, allow_private))
        .collect()
}

/// JOIN 形の木の展開（UNION ALL）: 深さごとの行の多重集合。アンカーは根（`parent_id IS NULL`）。
fn tree_levels<'a>(rows: &[&'a Truth]) -> Vec<Vec<&'a Truth>> {
    let mut levels: Vec<Vec<&Truth>> =
        vec![rows.iter().copied().filter(|t| t.b.is_none()).collect()];
    while !levels.last().expect("level").is_empty() {
        assert!(levels.len() < 30, "oracle: tree must be acyclic");
        let prev = levels.last().expect("level");
        let next: Vec<&Truth> = rows
            .iter()
            .copied()
            .flat_map(|c| {
                prev.iter()
                    .filter(move |p| c.b == Some(p.a))
                    .map(move |_| c)
            })
            .collect();
        levels.push(next);
    }
    levels.pop();
    levels
}

fn pair(t: &Truth) -> Vec<String> {
    vec![t.a.to_string(), t.label()]
}

fn sorted_pairs(mut v: Vec<&Truth>) -> Vec<&Truth> {
    v.sort_by_key(|x| (x.a, x.label()));
    v
}

/// 深さ優先の経路列（`{(1),(2)}`）。行は経路の辞書順（先行順）。重複行（同じ node_id の可視行が複数）は
/// 経路が同一の行として複数回現れる（同順位の行は同じ内容なので順序の不定は比較に影響しない）。
fn dfs_paths(rows: &[&Truth]) -> Vec<Vec<String>> {
    fn walk(rows: &[&Truth], node: &Truth, path: &mut Vec<i64>, out: &mut Vec<Vec<i64>>) {
        path.push(node.a);
        out.push(path.clone());
        for kid in rows.iter().filter(|c| c.b == Some(node.a)) {
            walk(rows, kid, path, out);
        }
        path.pop();
    }
    let mut paths: Vec<Vec<i64>> = Vec::new();
    for root in rows.iter().filter(|t| t.b.is_none()) {
        walk(rows, root, &mut Vec::new(), &mut paths);
    }
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let cells: Vec<String> = p.iter().map(|n| format!("({n})")).collect();
            vec![
                p.last().expect("non-empty").to_string(),
                format!("{{{}}}", cells.join(",")),
            ]
        })
        .collect()
}

/// 辺の展開。`union` なら重複除去（到達集合）、でなければ多重集合。反復が `cap` を超えたら `None`。
fn walk_edges(edges: &[&Truth], start: i64, union: bool, cap: usize) -> Option<Vec<i64>> {
    let mut all = vec![start];
    let mut seen: BTreeSet<i64> = BTreeSet::from([start]);
    let mut work = vec![start];
    let mut iterations = 0;
    while !work.is_empty() {
        iterations += 1;
        if iterations > cap {
            return None;
        }
        let mut next: Vec<i64> = Vec::new();
        for e in edges {
            for w in &work {
                if e.a == *w {
                    next.push(e.b.expect("dst"));
                }
            }
        }
        if union {
            next.retain(|n| seen.insert(*n));
            next.sort_unstable();
            next.dedup();
        }
        all.extend(next.iter().copied());
        work = next;
    }
    all.sort_unstable();
    Some(all)
}

fn col(values: impl IntoIterator<Item = i64>) -> Vec<Vec<String>> {
    values.into_iter().map(|v| vec![v.to_string()]).collect()
}

// ---------------------------------------------------------------------------
// SQL 形のマトリクス
// ---------------------------------------------------------------------------

const JOIN_CTE: &str = "WITH RECURSIVE t(n, label) AS (\
    SELECT node_id, label FROM tree WHERE parent_id IS NULL \
    UNION ALL \
    SELECT c.node_id, c.label FROM tree c JOIN t ON c.parent_id = t.n) ";
const JOIN_CTE_UNION: &str = "WITH RECURSIVE t(n, label) AS (\
    SELECT node_id, label FROM tree WHERE parent_id IS NULL \
    UNION \
    SELECT c.node_id, c.label FROM tree c JOIN t ON c.parent_id = t.n) ";
const KEY_CTE: &str = "WITH RECURSIVE t(n) AS (\
    SELECT node_id FROM tree WHERE parent_id IS NULL \
    UNION ALL \
    SELECT c.node_id FROM tree c JOIN t ON c.parent_id = t.n) ";

struct Shape {
    name: &'static str,
    /// 軸（T0 で件数を固定する）。
    axis: &'static str,
    sql: String,
    /// 閲覧者の見える行から期待応答を計算する独立オラクル。
    oracle: fn(&[Truth], &str, bool) -> Resp,
}

fn shapes() -> Vec<Shape> {
    let scan = "WITH RECURSIVE t(n, d) AS (\
        SELECT node_id, CAST(0 AS BIGINT) FROM tree WHERE parent_id IS NULL \
        UNION ALL SELECT n + 1, d + 1 FROM t WHERE d < 3) \
        SELECT n, d FROM t ORDER BY d, n";
    vec![
        Shape {
            name: "scan-form",
            axis: "scan",
            sql: scan.to_string(),
            oracle: |truth, v, p| {
                let roots: Vec<i64> = visible(truth, TREE, v, p)
                    .iter()
                    .filter(|t| t.b.is_none())
                    .map(|t| t.a)
                    .collect();
                let mut out: Vec<(i64, i64)> = Vec::new();
                for d in 0..=3 {
                    out.extend(roots.iter().map(|r| (d, r + d)));
                }
                out.sort_unstable();
                Ok(out
                    .into_iter()
                    .map(|(d, n)| vec![n.to_string(), d.to_string()])
                    .collect())
            },
        },
        Shape {
            name: "join-union-all",
            axis: "join",
            sql: format!("{JOIN_CTE}SELECT n, label FROM t ORDER BY n, label"),
            oracle: |truth, v, p| {
                let rows = visible(truth, TREE, v, p);
                let all: Vec<&Truth> = tree_levels(&rows).into_iter().flatten().collect();
                Ok(sorted_pairs(all).into_iter().map(pair).collect())
            },
        },
        Shape {
            name: "join-union",
            axis: "union",
            sql: format!("{JOIN_CTE_UNION}SELECT n, label FROM t ORDER BY n, label"),
            oracle: |truth, v, p| {
                let rows = visible(truth, TREE, v, p);
                let mut all: Vec<Vec<String>> =
                    tree_levels(&rows).into_iter().flatten().map(pair).collect();
                all.sort_by(|a, b| {
                    (a[0].parse::<i64>().expect("n"), &a[1])
                        .cmp(&(b[0].parse::<i64>().expect("n"), &b[1]))
                });
                all.dedup();
                Ok(all)
            },
        },
        Shape {
            name: "search-depth-first",
            axis: "search-depth",
            sql: format!("{KEY_CTE}SEARCH DEPTH FIRST BY n SET o SELECT n, o FROM t ORDER BY o"),
            oracle: |truth, v, p| Ok(dfs_paths(&visible(truth, TREE, v, p))),
        },
        Shape {
            name: "search-breadth-first",
            axis: "search-breadth",
            sql: format!("{KEY_CTE}SEARCH BREADTH FIRST BY n SET o SELECT n, o FROM t ORDER BY o"),
            oracle: |truth, v, p| {
                let rows = visible(truth, TREE, v, p);
                let mut out: Vec<(usize, i64)> = Vec::new();
                for (d, level) in tree_levels(&rows).iter().enumerate() {
                    out.extend(level.iter().map(|t| (d, t.a)));
                }
                out.sort_unstable();
                Ok(out
                    .into_iter()
                    .map(|(d, n)| vec![n.to_string(), format!("({d},{n})")])
                    .collect())
            },
        },
        Shape {
            name: "outer-aggregate",
            axis: "outer",
            sql: format!("{KEY_CTE}SELECT count(*), sum(n), max(n) FROM t"),
            oracle: |truth, v, p| {
                let rows = visible(truth, TREE, v, p);
                let ns: Vec<i64> = tree_levels(&rows)
                    .into_iter()
                    .flatten()
                    .map(|t| t.a)
                    .collect();
                Ok(vec![vec![
                    ns.len().to_string(),
                    ns.iter().sum::<i64>().to_string(),
                    ns.iter().max().expect("non-empty").to_string(),
                ]])
            },
        },
        Shape {
            name: "outer-where-order-limit",
            axis: "outer",
            sql: format!("{KEY_CTE}SELECT n FROM t WHERE n > 2 ORDER BY n DESC LIMIT 4"),
            oracle: |truth, v, p| {
                let rows = visible(truth, TREE, v, p);
                let mut ns: Vec<i64> = tree_levels(&rows)
                    .into_iter()
                    .flatten()
                    .map(|t| t.a)
                    .filter(|n| *n > 2)
                    .collect();
                ns.sort_unstable_by(|a, b| b.cmp(a));
                ns.truncate(4);
                Ok(col(ns))
            },
        },
        Shape {
            name: "edges-union-reachability",
            axis: "edges",
            sql: "WITH RECURSIVE g(n) AS (SELECT CAST(1 AS BIGINT) UNION \
                  SELECT e.dst FROM edges e JOIN g ON e.src = g.n) SELECT n FROM g ORDER BY n"
                .to_string(),
            oracle: |truth, v, p| match walk_edges(&visible(truth, EDGES, v, p), 1, true, 30) {
                Some(ns) => Ok(col(ns)),
                None => Err(("54000".to_string(), String::new())),
            },
        },
        Shape {
            name: "edges-union-all-cycle",
            axis: "edges",
            sql: "WITH RECURSIVE g(n) AS (SELECT CAST(10 AS BIGINT) UNION ALL \
                  SELECT e.dst FROM edges e JOIN g ON e.src = g.n) SELECT n FROM g ORDER BY n"
                .to_string(),
            oracle: |truth, v, p| match walk_edges(&visible(truth, EDGES, v, p), 10, false, 30) {
                Some(ns) => Ok(col(ns)),
                None => Err(("54000".to_string(), String::new())),
            },
        },
    ]
}

/// 期待応答（オラクル）と実応答の比較。失敗は `wire_code` だけを比べる（文面は T3 で別に確かめる）。
fn assert_matches(actual: &Resp, expected: &Resp, ctx_msg: &str) {
    match (actual, expected) {
        (Ok(a), Ok(e)) => assert_eq!(a, e, "{ctx_msg}"),
        (Err((a, _)), Err((e, _))) => assert_eq!(a, e, "{ctx_msg}"),
        _ => panic!("{ctx_msg}: actual={actual:?} expected={expected:?}"),
    }
}

fn foreign(viewer: &str) -> Vec<&'static str> {
    [ALICE, BOB, CAROL]
        .into_iter()
        .filter(|t| *t != viewer)
        .collect()
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

/// T0: 全形が baseline で受理され、結果が空でない。軸ごとの件数も固定する。
#[test]
fn t0_all_shapes_are_accepted_and_non_empty() {
    let (core, _g) = build_core(false, Cfg::Default);
    let all = shapes();
    for axis in [
        ("scan", 1),
        ("join", 1),
        ("union", 1),
        ("search-depth", 1),
        ("search-breadth", 1),
        ("outer", 2),
        ("edges", 2),
    ] {
        assert_eq!(
            all.iter().filter(|s| s.axis == axis.0).count(),
            axis.1,
            "axis {}",
            axis.0
        );
    }
    for s in &all {
        for (viewer, p) in VIEWERS {
            let resp = run(&core, viewer, p, &s.sql);
            let rows = resp.unwrap_or_else(|e| panic!("{} {viewer}/{p}: {e:?}", s.name));
            assert!(!rows.is_empty(), "{} {viewer}/{p}: empty", s.name);
        }
    }
    // alice の Private 行 8 は alice の Private モードでだけ見える（モード差が実在する）。
    let join = format!("{JOIN_CTE}SELECT n FROM t ORDER BY n");
    let pub_rows = run(&core, ALICE, false, &join).expect("ok");
    let prv_rows = run(&core, ALICE, true, &join).expect("ok");
    assert_eq!(prv_rows.len(), pub_rows.len() + 1);
}

/// T1: 全形・両状態・全閲覧者で、独立オラクルと一致し、不可視行のトークンが現れない。
#[test]
fn t1_results_match_independent_oracle() {
    for flooded in [false, true] {
        let truth = fixture(flooded);
        let (core, _g) = build_core(flooded, Cfg::Default);
        for s in shapes() {
            for (viewer, p) in VIEWERS {
                let ctx_msg = format!("{} flooded={flooded} {viewer}/{p}", s.name);
                let actual = run(&core, viewer, p, &s.sql);
                assert_matches(&actual, &(s.oracle)(&truth, viewer, p), &ctx_msg);
                assert_no_leak(
                    &actual,
                    &forbidden_labels(&truth, viewer, p),
                    &foreign(viewer),
                    &ctx_msg,
                );
            }
        }
    }
}

/// T2: 他テナントの Private 行を足しても、閲覧者（alice・carol）の応答は完全一致する。
/// carol（Public しか見えない対照）の応答は alice の Public モードと一致する（他テナントの存在を観測できない）。
#[test]
fn t2_flooding_foreign_private_rows_changes_nothing() {
    let (base, _g1) = build_core(false, Cfg::Default);
    let (flood, _g2) = build_core(true, Cfg::Default);
    for s in shapes() {
        for (viewer, p) in [(ALICE, false), (ALICE, true), (CAROL, false), (CAROL, true)] {
            let b = run(&base, viewer, p, &s.sql);
            let f = run(&flood, viewer, p, &s.sql);
            assert_eq!(b, f, "{} {viewer}/{p}", s.name);
        }
        // carol は alice の Private 行を持たないので、Public 閲覧の alice と一致する。
        for core in [&base, &flood] {
            assert_eq!(
                run(core, CAROL, true, &s.sql),
                run(core, ALICE, false, &s.sql),
                "{}",
                s.name
            );
        }
    }
}

/// 逆向き: bob が見る木には alice の Private 行（8）が現れない。bob 自身の Private 子は到達できる。
#[test]
fn reverse_direction_hides_alice_private_branch() {
    let truth = fixture(true);
    let (core, _g) = build_core(true, Cfg::Default);
    let sql = format!("{KEY_CTE}SELECT n FROM t ORDER BY n");
    for p in [false, true] {
        let rows = run(&core, BOB, p, &sql).expect("ok");
        let ns: Vec<i64> = rows.iter().map(|r| r[0].parse().expect("n")).collect();
        let visible_rows = visible(&truth, TREE, BOB, p);
        let expected: Vec<i64> = {
            let mut v: Vec<i64> = tree_levels(&visible_rows)
                .into_iter()
                .flatten()
                .map(|t| t.a)
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(ns, expected, "bob/{p}");
    }
}

/// T3 (a): 静的エラー（型不一致・不正な再帰・SEARCH と UNION の併用）は、他テナント行の有無で
/// `wire_code`・`client_message` とも変わらず、他テナントの情報を含まない。
#[test]
fn t3a_static_errors_are_identical_and_clean() {
    let (base, _g1) = build_core(false, Cfg::Default);
    let (flood, _g2) = build_core(true, Cfg::Default);
    let cases = [
        (
            "42804",
            "WITH RECURSIVE t(n) AS (SELECT node_id FROM tree WHERE parent_id IS NULL \
             UNION ALL SELECT n + 1, 2 FROM t WHERE n < 3) SELECT n FROM t",
        ),
        (
            "42P19",
            "WITH RECURSIVE t(n) AS (SELECT n FROM t) SELECT n FROM t",
        ),
        (
            "0A000",
            "WITH RECURSIVE t(n) AS (SELECT node_id FROM tree WHERE parent_id IS NULL \
             UNION SELECT c.node_id FROM tree c JOIN t ON c.parent_id = t.n) \
             SEARCH DEPTH FIRST BY n SET o SELECT n FROM t",
        ),
    ];
    let truth = fixture(true);
    for (expected, sql) in cases {
        for (viewer, p) in VIEWERS {
            let b = run(&base, viewer, p, sql);
            let f = run(&flood, viewer, p, sql);
            assert_eq!(b, f, "{expected} {viewer}/{p}");
            let err = f.as_ref().expect_err("must fail");
            assert_eq!(err.0, expected, "{viewer}/{p}: {sql}");
            assert_no_leak(
                &f,
                &forbidden_labels(&truth, viewer, p),
                &foreign(viewer),
                expected,
            );
        }
    }
}

/// 閲覧者自身の行で超過する `54000` の文面は、他テナントの Private 行の有無で変わらない。
#[test]
fn t3c_own_overflow_message_is_unchanged_by_foreign_rows() {
    let unbounded = "WITH RECURSIVE t(n) AS (SELECT node_id FROM tree WHERE parent_id IS NULL \
                     UNION ALL SELECT n FROM t) SELECT n FROM t";
    let truth = fixture(true);
    let (base, _g1) = build_core(false, Cfg::Default);
    let (flood, _g2) = build_core(true, Cfg::Default);
    for (viewer, p) in [(ALICE, false), (ALICE, true), (CAROL, false)] {
        let b = run(&base, viewer, p, unbounded);
        let f = run(&flood, viewer, p, unbounded);
        assert_eq!(b.as_ref().expect_err("54000").0, "54000");
        assert_eq!(b, f, "{viewer}/{p}");
        assert_no_leak(
            &f,
            &forbidden_labels(&truth, viewer, p),
            &foreign(viewer),
            "own overflow",
        );
    }
}

/// 上限の canary: 他テナントの Private 行だけが超過の原因になる設定で、閲覧者（alice・carol）の応答は
/// baseline と同じ成功のまま。陽性対照として、bob 自身（flooded）では同じ設定で `54000` になる。
fn canary(
    name: &str,
    sql: &str,
    margin: usize,
    loose: impl Fn(usize) -> Cfg,
    count: impl Fn(&[Truth], &str, bool) -> usize,
) {
    let truth = fixture(true);
    // alice が全モードで必要とする最大量に、小さな余裕を足した上限にする。
    let alice_max = [false, true]
        .into_iter()
        .map(|p| count(&truth, ALICE, p))
        .max()
        .expect("max");
    let bob_need = count(&truth, BOB, true);
    assert!(
        bob_need > alice_max + margin + 3,
        "{name}: fixture must make bob exceed the limit (bob={bob_need}, alice={alice_max})"
    );
    let cfg = loose(alice_max + margin);
    let (base, _g1) = build_core(false, cfg);
    let (flood, _g2) = build_core(true, cfg);
    for (viewer, p) in [(ALICE, false), (ALICE, true), (CAROL, false), (CAROL, true)] {
        let b = run(&base, viewer, p, sql);
        let f = run(&flood, viewer, p, sql);
        assert!(b.is_ok(), "{name} baseline {viewer}/{p}: {b:?}");
        assert_eq!(b, f, "{name} {viewer}/{p}");
        assert_no_leak(
            &f,
            &forbidden_labels(&truth, viewer, p),
            &foreign(viewer),
            name,
        );
    }
    // 陽性対照。
    let control = run(&flood, BOB, true, sql);
    assert_eq!(
        control.as_ref().expect_err("positive control").0,
        "54000",
        "{name}"
    );
    assert_no_leak(
        &control,
        &forbidden_labels(&truth, BOB, true),
        &foreign(BOB),
        name,
    );
    assert!(
        run(&base, BOB, true, sql).is_ok(),
        "{name}: bob baseline must succeed"
    );
    // 閲覧者自身の行で超過する設定（厳しい上限）の文面は baseline と flooded で一致する。
    let (tb, _g3) = build_core(false, loose(2));
    let (tf, _g4) = build_core(true, loose(2));
    let b = run(&tb, ALICE, true, sql);
    assert_eq!(b.as_ref().expect_err("tight").0, "54000", "{name} tight");
    assert_eq!(b, run(&tf, ALICE, true, sql), "{name} tight");
}

fn join_count(truth: &[Truth], viewer: &str, p: bool) -> usize {
    tree_levels(&visible(truth, TREE, viewer, p))
        .into_iter()
        .flatten()
        .count()
}

fn tree_rows(truth: &[Truth], viewer: &str, p: bool) -> usize {
    visible(truth, TREE, viewer, p).len()
}

/// 反復回数: bob の Private な長い鎖だけが上限を超える。
#[test]
fn t3b_iteration_limit_canary() {
    let sql = format!("{KEY_CTE}SELECT count(*) FROM t");
    canary("iterations", &sql, 2, Cfg::Iter, |truth, v, p| {
        tree_levels(&visible(truth, TREE, v, p)).len()
    });
}

/// 累積行数: bob の Private な扇状の展開だけが上限を超える。
#[test]
fn t3b_row_limit_canary() {
    let sql = format!("{KEY_CTE}SELECT count(*) FROM t");
    canary("rows", &sql, 2, Cfg::Rows, join_count);
}

/// JOIN の入力行数: 基底表側の bob の Private 行だけで入力行数が上限を超える。
#[test]
fn t3b_join_input_limit_canary() {
    let sql = format!("{KEY_CTE}SELECT count(*) FROM t");
    canary("join-input", &sql, 2, Cfg::JoinInput, tree_rows);
}

/// `--max-result-rows`: bob の行だけで結果行数の上限を超える。
#[test]
fn t3b_result_limit_canary() {
    let sql = format!("{JOIN_CTE}SELECT n, label FROM t");
    canary("result-rows", &sql, 2, Cfg::Result, join_count);
}

/// UNION ALL の循環: bob の Private な辺だけが循環を閉じる。alice は停止して成功し、
/// 陽性対照の bob は `54000`（T1 のオラクルでも同じ判定を確認済み）。
#[test]
fn t3b_union_all_cycle_closed_only_by_foreign_edge() {
    let sql = "WITH RECURSIVE g(n) AS (SELECT CAST(10 AS BIGINT) UNION ALL \
               SELECT e.dst FROM edges e JOIN g ON e.src = g.n) SELECT n FROM g ORDER BY n";
    let (flood, _g) = build_core(true, Cfg::Default);
    assert_eq!(run(&flood, ALICE, false, sql), Ok(col([10, 11, 12])));
    assert_eq!(run(&flood, ALICE, true, sql), Ok(col([10, 11, 12, 13])));
    assert_eq!(run(&flood, CAROL, true, sql), Ok(col([10, 11, 12])));
    assert_eq!(run(&flood, BOB, true, sql).expect_err("cycle").0, "54000");
    // 同じ辺を alice が持つ場合の UNION は停止する（重複除去）。
    let union_sql = sql.replace("UNION ALL", "UNION");
    assert_eq!(run(&flood, BOB, true, &union_sql), Ok(col([10, 11, 12])));
}

/// T5: キャッシュの温め順序（他テナント・他モードで先に実行しても）で応答が変わらない。
#[test]
fn t5_warm_order_does_not_change_responses() {
    let targets: Vec<String> = shapes()
        .into_iter()
        .filter(|s| matches!(s.axis, "join" | "search-breadth" | "search-depth"))
        .map(|s| s.sql)
        .collect();
    for sql in &targets {
        let (fresh, _g1) = build_core(true, Cfg::Default);
        let alone = run(&fresh, ALICE, true, sql);
        let (warm, _g2) = build_core(true, Cfg::Default);
        for (v, p) in [(BOB, false), (BOB, true), (CAROL, true)] {
            let _ = run(&warm, v, p, sql);
        }
        assert_eq!(run(&warm, ALICE, true, sql), alone, "bob-first: {sql}");
        let (seq, _g3) = build_core(true, Cfg::Default);
        let first = run(&seq, ALICE, true, sql);
        let _ = run(&seq, BOB, true, sql);
        assert_eq!(run(&seq, ALICE, true, sql), first, "A-B-A: {sql}");
        assert_eq!(first, alone, "{sql}");
    }
}

/// T6: 検査器が違反を見逃さない（捏造した漏洩・食い違いを検出して panic する）。
#[test]
fn t6_negative_controls_are_detected() {
    let truth = fixture(true);
    let forbidden = forbidden_labels(&truth, ALICE, true);
    let leaked = truth
        .iter()
        .find(|t| t.table == TREE && t.tenant == BOB && matches!(t.vis, Visibility::Private))
        .expect("bob private row");
    let forged: Resp = Ok(vec![vec![leaked.a.to_string(), leaked.label()]]);
    let r = catch_unwind(AssertUnwindSafe(|| {
        assert_no_leak(&forged, &forbidden, &foreign(ALICE), "forged");
    }));
    assert!(r.is_err(), "leaked token must be detected");
    let msg: Resp = Err(("54000".to_string(), "too many rows from bob".to_string()));
    let r = catch_unwind(AssertUnwindSafe(|| {
        assert_no_leak(&msg, &forbidden, &foreign(ALICE), "forged message");
    }));
    assert!(r.is_err(), "tenant name in error must be detected");
    // オラクル比較: 他テナントの Private 行を 1 行足した結果は不一致になる。
    let s = &shapes()[1];
    let expected = (s.oracle)(&truth, ALICE, true);
    let mut tampered = expected.clone().expect("ok");
    tampered.push(vec![leaked.a.to_string(), leaked.label()]);
    let r = catch_unwind(AssertUnwindSafe(|| {
        assert_matches(&Ok(tampered), &expected, "tampered");
    }));
    assert!(r.is_err(), "extra row must be detected");
    // 禁止トークンが空振りでないこと（flooded の alice には 30 行以上の不可視行がある）。
    assert!(forbidden.len() > 30);
}
