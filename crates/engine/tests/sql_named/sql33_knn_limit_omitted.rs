//! 距離順 `ORDER BY` の `LIMIT` 省略の受け入れテスト（Issue #2242。ビヘイビア: SQL-33 (2)(8)・
//! SQL-34 (2)(3)(4)・SQL-75 (4)。ポインタ: `docs/spec/04-behavior/sql-surface.md`）。
//!
//! 固定する契約:
//! - `LIMIT` 省略・`LIMIT ALL`・`OFFSET` 単独は ANN 索引を使わず可視行を厳密全走査し、結果は
//!   「全可視行を同じスコア関数で距離順（同点は行の順序キー昇順）に安定ソートしたもの」と一致する。
//! - 可視件数（`OFFSET` 適用後）が結果行数上限を超えたら、行を返さず `54000` で文全体を拒否する。
//!   他テナントの行は計数に入らない。
//! - `USING MODE 'precision'` との併用は `0A000`。hybrid 関数形・`USING PLAN` の省略は `42601`のまま。
//! - `EXPLAIN` は `ann_plan: plain_scan_limit_omitted` を出し、`index=` 注記を付けない。
//!
//! 距離種別の追加（#1981・#1991）に備え、一致検査は [`Metric`]（演算子とスコア関数の組）で
//! パラメータ化している。まだ SQL から実行できない種別は、省略しても明示 `LIMIT` と同じ
//! エラーコードで拒否されること（fail-closed）だけを確かめる。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::search_engine;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 3;

/// 距離演算子と、その昇順経路が使うスコア（大きいほど近い）の組。
struct Metric {
    op: &'static str,
    score: fn(&[f32], &[f32]) -> f32,
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// 現時点で SQL から実行できる距離種別。後続 Issue が種別を足すときはここへ追記する。
fn runnable_metrics() -> Vec<Metric> {
    vec![Metric {
        op: "<=>",
        score: dot,
    }]
}

/// (id, embedding, lang)。成分は 0.5 の倍数で、どの加算順序でも内積が厳密になる。
/// 1 と 3、2 と 7、5 と 6 は問い合わせ `[1,0,0]` で同点。
fn rows() -> Vec<(u64, [f32; DIM], &'static str)> {
    vec![
        (1, [0.5, 0.0, 0.0], "ja"),
        (2, [-1.0, 0.0, 0.0], "en"),
        (3, [0.5, 1.0, 0.0], "ja"),
        (4, [2.0, 0.0, 0.0], "en"),
        (5, [0.0, 0.0, 0.0], "ja"),
        (6, [0.0, 1.0, 0.0], "en"),
        (7, [-1.0, 0.5, 0.0], "ja"),
        (8, [3.0, 0.0, 0.5], "en"),
    ]
}

/// 既定の可視件数（`rows()` の件数）。これと同じ cap なら成功、1 つ小さい cap なら `54000`。
const N: usize = 8;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_storage(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    storage
}

fn seed_tenant(core: &EngineCore, tenant: &str, rows: &[(u64, [f32; DIM], &str)]) {
    let ctx = ctx(tenant);
    let mut session = SessionState::default();
    for (id, v, lang) in rows {
        let sql = format!(
            "INSERT INTO documents (id, embedding, body, lang) VALUES ({id}, '[{},{},{}]', 'b{id}', '{lang}') USING OPERATION_ID 'seed-{tenant}-{id}'",
            v[0], v[1], v[2]
        );
        core.execute_sql_in_session(&ctx, &mut session, &sql)
            .unwrap_or_else(|e| panic!("seed {sql}: {e:?}"));
    }
}

fn seed(core: &EngineCore) {
    seed_tenant(core, "tenant-a", &rows());
}

fn new_core(path: &std::path::Path, cap: usize) -> EngineCore {
    EngineCore::from_storage(open_storage(path), Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
        .with_result_limits(ResultLimits::new(cap))
}

fn new_hnsw_core(path: &std::path::Path, cap: usize) -> EngineCore {
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    EngineCore::from_storage_with_engine(open_storage(path), kind)
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
        .with_result_limits(ResultLimits::new(cap))
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    {
        SqlOutcome::Query(result) => result,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn run_err_code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql) {
        Err(e) => e.wire_code().to_string(),
        Ok(_) => panic!("{sql} must fail"),
    }
}

fn ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

/// テスト側の全走査モデル: 全行を同じスコア関数で計算し、近い順（スコア降順）または遠い順
/// （スコア昇順）、同点は id 昇順に安定ソートして先頭 `offset` 件を捨てた id 列を返す。
fn model(
    metric: &Metric,
    q: &[f32; DIM],
    descending: bool,
    keep: impl Fn(&str) -> bool,
    offset: usize,
) -> Vec<u64> {
    let mut all: Vec<(u64, f32)> = rows()
        .into_iter()
        .filter(|(_, _, lang)| keep(lang))
        .map(|(id, v, _)| (id, (metric.score)(&v, q)))
        .collect();
    all.sort_by(|a, b| {
        let by_score = if descending {
            a.1.total_cmp(&b.1)
        } else {
            b.1.total_cmp(&a.1)
        };
        by_score.then(a.0.cmp(&b.0))
    });
    all.into_iter().skip(offset).map(|(id, _)| id).collect()
}

fn check_matches_exact_scan(core: &EngineCore, metric: &Metric) {
    let q = [1.0f32, 0.0, 0.0];
    let lit = "[1,0,0]";
    for (dir_sql, descending) in [("", false), (" ASC", false), (" DESC", true)] {
        for (tail, offset) in [
            ("", 0usize),
            (" LIMIT ALL", 0),
            (" OFFSET 3", 3),
            (" LIMIT ALL OFFSET 3", 3),
        ] {
            let sql = format!(
                "SELECT id FROM documents ORDER BY embedding {} '{lit}'{dir_sql}{tail}",
                metric.op
            );
            assert_eq!(
                ids(&run(core, "tenant-a", &sql)),
                model(metric, &q, descending, |_| true, offset),
                "{sql}"
            );
            // WHERE あり（等価述語）。SCALAR 先行の厳密全走査と一致する。
            let sql = format!(
                "SELECT id FROM documents WHERE lang = 'ja' ORDER BY embedding {} '{lit}'{dir_sql}{tail}",
                metric.op
            );
            assert_eq!(
                ids(&run(core, "tenant-a", &sql)),
                model(metric, &q, descending, |l| l == "ja", offset.min(4)),
                "{sql}"
            );
            // HINT ORDER で DISTANCE を先にしても、LIMIT 省略では SCALAR 先へ倒れ同じ結果になる。
            let hinted = format!("{sql} HINT ORDER(DISTANCE, SCALAR, RLS)");
            assert_eq!(
                ids(&run(core, "tenant-a", &hinted)),
                ids(&run(core, "tenant-a", &sql)),
                "{hinted}"
            );
        }
    }
}

// SQL-34: LIMIT 省略・LIMIT ALL・OFFSET 単独の結果が全可視行の厳密な距離順と一致する。
#[test]
fn sql33_limit_omitted_matches_exact_full_scan() {
    let path = unique_db_path("sql33-omit-exact");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path, N);
    seed(&core);
    for metric in runnable_metrics() {
        check_matches_exact_scan(&core, &metric);
    }
}

// HNSW opt-in のエンジンでも省略は索引を使わず厳密全走査と一致する。
#[test]
fn sql33_limit_omitted_matches_exact_full_scan_on_hnsw_engine() {
    let path = unique_db_path("sql33-omit-hnsw");
    let _guard = CleanupGuard(path.clone());
    let core = new_hnsw_core(&path, N);
    seed(&core);
    for metric in runnable_metrics() {
        check_matches_exact_scan(&core, &metric);
    }
}

// SQL-34 (3): 可視件数が cap ちょうどなら成功、cap + 1 なら 54000 で行を返さない。
#[test]
fn sql34_limit_omitted_cap_boundary_is_exact() {
    let sql = "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]'";
    for variant in ["", " LIMIT ALL", " DESC"] {
        let path = unique_db_path("sql34-omit-boundary-ok");
        let _guard = CleanupGuard(path.clone());
        let ok = new_core(&path, N);
        seed(&ok);
        assert_eq!(
            run(&ok, "tenant-a", &format!("{sql}{variant}")).rows.len(),
            N
        );

        let path = unique_db_path("sql34-omit-boundary-ng");
        let _guard2 = CleanupGuard(path.clone());
        let over = new_core(&path, N - 1);
        seed(&over);
        assert_eq!(
            run_err_code(&over, "tenant-a", &format!("{sql}{variant}")),
            "54000",
            "{variant}"
        );
    }
}

// OFFSET は可視行のみを数える: `offset + cap` 件までは成功、`offset + cap + 1` 件目で 54000。
#[test]
fn sql34_limit_omitted_offset_is_applied_before_the_cap() {
    let path = unique_db_path("sql34-omit-offset");
    let _guard = CleanupGuard(path.clone());
    // 可視 8 件・cap 5。OFFSET 3 なら残り 5 件で成功、OFFSET 2 なら残り 6 件で 54000。
    let core = new_core(&path, 5);
    seed(&core);
    let ok = run(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' OFFSET 3",
    );
    assert_eq!(ok.rows.len(), 5);
    assert_eq!(
        run_err_code(
            &core,
            "tenant-a",
            "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' OFFSET 2",
        ),
        "54000"
    );
    // OFFSET が検索上限（MAX_SEARCH_K）を超える場合は 22000。
    assert_eq!(
        run_err_code(
            &core,
            "tenant-a",
            "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' OFFSET 4294967295",
        ),
        "22000"
    );
}

// RLS: 他テナントの行は cap の計数にも結果にも入らない。
#[test]
fn sql34_limit_omitted_ignores_other_tenants_rows() {
    let path = unique_db_path("sql34-omit-tenant");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path, N);
    seed(&core);
    let others: Vec<(u64, [f32; DIM], &str)> = (100..140)
        .map(|id| (id, [-(id as f32), 0.0, 0.0], "ja"))
        .collect();
    seed_tenant(&core, "tenant-b", &others);
    for variant in ["", " DESC"] {
        let a = run(
            &core,
            "tenant-a",
            &format!("SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]'{variant}"),
        );
        assert_eq!(a.rows.len(), N, "{variant}");
        assert!(ids(&a).iter().all(|id| *id < 100), "{:?}", ids(&a));
    }
    // 自テナントが cap を超えたテナントだけが 54000 になる（件数の存在は他テナントに漏れない）。
    assert_eq!(
        run_err_code(
            &core,
            "tenant-b",
            "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]'",
        ),
        "54000"
    );
}

// WHERE と省略の併用: 絞り込み後の件数が cap 以内なら成功、超えたら 54000。
#[test]
fn sql34_limit_omitted_cap_counts_rows_after_where() {
    let path = unique_db_path("sql34-omit-where");
    let _guard = CleanupGuard(path.clone());
    // lang = 'ja' は 4 件。cap 4 で成功、全行（8 件）は 54000。
    let core = new_core(&path, 4);
    seed(&core);
    let ja = run(
        &core,
        "tenant-a",
        "SELECT id FROM documents WHERE lang = 'ja' ORDER BY embedding <=> '[1,0,0]'",
    );
    assert_eq!(ja.rows.len(), 4);
    // DISTANCE 先行のヒントでも、事後フィルタで件数が減って超過を見逃さない。
    let hinted = run(
        &core,
        "tenant-a",
        "SELECT id FROM documents WHERE lang = 'ja' ORDER BY embedding <=> '[1,0,0]' HINT ORDER(DISTANCE, SCALAR, RLS)",
    );
    assert_eq!(ids(&hinted), ids(&ja));
    assert_eq!(
        run_err_code(
            &core,
            "tenant-a",
            "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]'",
        ),
        "54000"
    );
}

// precision との併用は 0A000（クエリ句・セッション変数とも）。
#[test]
fn sql33_limit_omitted_with_precision_mode_is_feature_not_supported() {
    let path = unique_db_path("sql33-omit-precision");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path, N);
    seed(&core);
    assert_eq!(
        run_err_code(
            &core,
            "tenant-a",
            "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' USING MODE 'precision'",
        ),
        "0A000"
    );
    // 明示 LIMIT の precision は従来どおり受理される。
    run(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3 USING MODE 'precision'",
    );
}

// hybrid 関数形と USING PLAN の LIMIT 省略は従来どおり 42601。
#[test]
fn sql33_limit_omitted_stays_syntax_error_for_hybrid_and_using_plan() {
    let path = unique_db_path("sql33-omit-syntax");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path, N);
    seed(&core);
    for sql in [
        "SELECT id FROM documents ORDER BY HYBRID(embedding, '[1,0,0]', 'q')",
        "SELECT id FROM documents ORDER BY HYBRID(embedding, '[1,0,0]', 'q') LIMIT ALL",
        "SELECT id FROM documents USING PLAN('q')",
    ] {
        assert_eq!(run_err_code(&core, "tenant-a", sql), "42601", "{sql}");
    }
}

// 明示 LIMIT の契約（LIMIT 0 / cap 超過は 22000）は変えない。
#[test]
fn sql34_explicit_limit_contract_is_unchanged() {
    let path = unique_db_path("sql34-omit-explicit");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path, N);
    seed(&core);
    for limit in ["0", &(N + 1).to_string()] {
        assert_eq!(
            run_err_code(
                &core,
                "tenant-a",
                &format!("SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT {limit}"),
            ),
            "22000",
            "LIMIT {limit}"
        );
    }
}

// まだ SQL から実行できない距離演算子は、省略しても明示 LIMIT と同じコードで拒否される。
#[test]
fn sql33_limit_omitted_on_not_yet_runnable_operators_fails_like_explicit_limit() {
    let path = unique_db_path("sql33-omit-unrunnable");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path, N);
    seed(&core);
    for op in ["<->", "<#>", "<+>"] {
        let explicit = run_err_code(
            &core,
            "tenant-a",
            &format!("SELECT id FROM documents ORDER BY embedding {op} '[1,0,0]' LIMIT 3"),
        );
        let omitted = run_err_code(
            &core,
            "tenant-a",
            &format!("SELECT id FROM documents ORDER BY embedding {op} '[1,0,0]'"),
        );
        assert_eq!(explicit, omitted, "{op}");
        assert_eq!(omitted, "0A000", "{op}");
    }
}

// 拡張クエリプロトコル: Describe が成功し、Parse・Bind・Execute がリテラル形と同じ結果を返す。
#[test]
fn sql33_limit_omitted_works_through_prepared_parameters() {
    let path = unique_db_path("sql33-omit-prepared");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path, N);
    seed(&core);
    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents ORDER BY embedding <=> $1")
        .expect("parse_sql_prepared");
    let session = SessionState::default();
    let columns = core
        .describe_prepared_in_session(&session, &prepared)
        .expect("describe");
    assert!(columns.is_some());
    let bound = core
        .bind_prepared(&prepared, &[Some(b"[1,0,0]".to_vec())])
        .expect("bind_prepared");
    let mut session = SessionState::default();
    let outcome = core
        .execute_parsed_in_session(&ctx("tenant-a"), &mut session, &bound)
        .expect("execute");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query");
    };
    let metric = &runnable_metrics()[0];
    assert_eq!(
        ids(&result),
        model(metric, &[1.0, 0.0, 0.0], false, |_| true, 0)
    );
}

fn explain_lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

// SQL-75 (4): EXPLAIN は索引を使わない理由（LIMIT 省略）を出し、索引名注記を付けない。
// DESC 併用は plain_scan_descending が優先し、明示 LIMIT の昇順は従来の出力のまま。
#[test]
fn sql75_explain_reports_plain_scan_limit_omitted() {
    for hnsw in [false, true] {
        let path = unique_db_path("sql75-omit-explain");
        let _guard = CleanupGuard(path.clone());
        let core = if hnsw {
            new_hnsw_core(&path, N)
        } else {
            new_core(&path, N)
        };
        seed(&core);
        for tail in ["", " LIMIT ALL", " OFFSET 2"] {
            let omitted = explain_lines(
                &core,
                "tenant-a",
                &format!("EXPLAIN SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]'{tail}"),
            );
            assert!(
                omitted
                    .iter()
                    .any(|l| l == "ann_plan: plain_scan_limit_omitted"),
                "hnsw={hnsw} tail={tail:?}: {omitted:?}"
            );
            assert!(!omitted.iter().any(|l| l.contains("index=")), "{omitted:?}");
        }
        let desc = explain_lines(
            &core,
            "tenant-a",
            "EXPLAIN SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC",
        );
        assert!(
            desc.iter().any(|l| l == "ann_plan: plain_scan_descending"),
            "hnsw={hnsw}: {desc:?}"
        );
        let explicit = explain_lines(
            &core,
            "tenant-a",
            "EXPLAIN SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3",
        );
        assert!(
            !explicit
                .iter()
                .any(|l| l.contains("plain_scan_limit_omitted")),
            "hnsw={hnsw}: {explicit:?}"
        );
    }
}
