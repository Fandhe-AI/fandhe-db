//! 距離順 `ORDER BY` の `DESC`（遠い順）の受け入れテスト（Issue #2009。ビヘイビア: SQL-33 (2)・
//! SQL-75 (4)。ポインタ: `docs/spec/04-behavior/sql-surface.md`）。
//!
//! 固定する契約:
//! - `DESC` は ANN 索引を使わず可視行を全走査し、結果は「全可視行を同じスコア関数で計算して
//!   遠い順（同点は行の順序キー昇順）に安定ソートした先頭 `k` 件」と一致する。
//! - `LIMIT` が行数以上なら全行が返る（黙って切らない）。明示の `ASC` は方向省略と同じ。
//! - 他テナントの行は結果に入らない。`precision` モードとの併用は `0A000`。
//! - `EXPLAIN` は `ann_plan: plain_scan_descending` を出し、`index=` 注記を付けない。
//!
//! 距離種別の追加（#1977 ほか）に備え、一致検査は [`Metric`]（演算子と距離関数の組）で
//! パラメータ化している。`<=>`（コサイン距離）と `<#>`（負の内積）は Issue #1991 で接続済み。
//! まだ SQL から実行できない種別は、`DESC` を付けても `ASC` と同じエラーコードで拒否されること
//! （fail-closed）だけを確かめる。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::search_engine;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 3;

/// 距離演算子と、その昇順経路が使う距離値（小さいほど近い。NaN は最も遠い）の組。
struct Metric {
    op: &'static str,
    distance: fn(&[f32], &[f32]) -> f64,
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// `<=>`: `1 - cos`（f64。ゼロノルムは NaN）。成分は 0.5 の倍数で、f32 の内積・ノルム²は厳密。
fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let (na, nb) = (f64::from(dot(a, a)), f64::from(dot(b, b)));
    if na * nb == 0.0 {
        return f64::NAN;
    }
    1.0 - (f64::from(dot(a, b)) / (na * nb).sqrt()).clamp(-1.0, 1.0)
}

/// `<#>`: 負の内積。
fn neg_inner_product(a: &[f32], b: &[f32]) -> f64 {
    -f64::from(dot(a, b))
}

/// 現時点で SQL から実行できる距離種別。後続 Issue が種別を足すときはここへ追記する。
fn runnable_metrics() -> Vec<Metric> {
    vec![
        Metric {
            op: "<=>",
            distance: cosine_distance,
        },
        Metric {
            op: "<#>",
            distance: neg_inner_product,
        },
    ]
}

/// 遠い順の比較（距離の降順。NaN は先頭。同点は id 昇順）。
fn far_first(a: &(u64, f64), b: &(u64, f64)) -> std::cmp::Ordering {
    match (a.1.is_nan(), b.1.is_nan()) {
        (true, true) => a.0.cmp(&b.0),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)),
    }
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

fn seed(core: &EngineCore) {
    seed_tenant(core, "tenant-a", &rows());
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

fn new_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(open_storage(path), Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
}

fn new_hnsw_core(path: &std::path::Path) -> EngineCore {
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    EngineCore::from_storage_with_engine(open_storage(path), kind)
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
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

fn query_literal(q: &[f32; DIM]) -> String {
    format!("[{},{},{}]", q[0], q[1], q[2])
}

/// テスト側の全走査モデル: 全行を同じ距離関数で計算し、遠い順（距離降順・NaN 先頭）・同点は id 昇順に
/// 安定ソートして先頭 `k` 件の id を返す。
fn model_desc(metric: &Metric, q: &[f32; DIM], keep: impl Fn(&str) -> bool, k: usize) -> Vec<u64> {
    let mut all: Vec<(u64, f64)> = rows()
        .into_iter()
        .filter(|(_, _, lang)| keep(lang))
        .map(|(id, v, _)| (id, (metric.distance)(&v, q)))
        .collect();
    all.sort_by(far_first);
    all.into_iter().take(k).map(|(id, _)| id).collect()
}

fn check_desc_matches_exact_scan(core: &EngineCore, metric: &Metric) {
    let q = [1.0f32, 0.0, 0.0];
    let lit = query_literal(&q);
    let total = rows().len();
    for k in [1usize, 3, 5, total, total + 10] {
        // WHERE なし（既定の評価順）
        let sql = format!(
            "SELECT id FROM documents ORDER BY embedding {} '{lit}' DESC LIMIT {k}",
            metric.op
        );
        assert_eq!(
            ids(&run(core, "tenant-a", &sql)),
            model_desc(metric, &q, |_| true, k),
            "{sql}"
        );
        // WHERE あり
        let sql = format!(
            "SELECT id FROM documents WHERE lang = 'ja' ORDER BY embedding {} '{lit}' DESC LIMIT {k}",
            metric.op
        );
        assert_eq!(
            ids(&run(core, "tenant-a", &sql)),
            model_desc(metric, &q, |l| l == "ja", k),
            "{sql}"
        );
        // HINT ORDER の 2 通り。SCALAR 先行は絞り込み後に k 件、DISTANCE 先行は全可視行の遠い順 k 件を
        // 取ってから絞り込む（昇順経路と同じ評価順の意味）。
        for hint in ["DISTANCE, RLS, SCALAR", "RLS, SCALAR, DISTANCE"] {
            let sql = format!(
                "SELECT id FROM documents WHERE lang = 'en' ORDER BY embedding {} '{lit}' DESC LIMIT {k} HINT ORDER({hint})",
                metric.op
            );
            let expected = if hint.starts_with("DISTANCE") {
                let en: Vec<u64> = rows()
                    .into_iter()
                    .filter(|(_, _, l)| *l == "en")
                    .map(|(id, _, _)| id)
                    .collect();
                model_desc(metric, &q, |_| true, k)
                    .into_iter()
                    .filter(|id| en.contains(id))
                    .collect()
            } else {
                model_desc(metric, &q, |l| l == "en", k)
            };
            assert_eq!(ids(&run(core, "tenant-a", &sql)), expected, "{sql}");
        }
    }
}

// SQL-33 (2): 実行できる全距離種別で DESC の結果が全走査の結果と一致する。
#[test]
fn sql33_desc_matches_exact_full_scan_for_every_runnable_metric() {
    let path = unique_db_path("sql33-desc-exact");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    seed(&core);
    for metric in runnable_metrics() {
        check_desc_matches_exact_scan(&core, &metric);
    }
}

// SQL-33 (2): HNSW opt-in のエンジンでも DESC は索引を使わず全走査と一致する。
#[test]
fn sql33_desc_matches_exact_full_scan_on_hnsw_engine() {
    let path = unique_db_path("sql33-desc-hnsw");
    let _guard = CleanupGuard(path.clone());
    let core = new_hnsw_core(&path);
    seed(&core);
    for metric in runnable_metrics() {
        check_desc_matches_exact_scan(&core, &metric);
    }
}

// SQL-33 (2)(3): LIMIT が行数以上なら全行が返り、DESC は距離降順（ゼロノルム行は NaN で先頭）。
#[test]
fn sql33_desc_with_large_limit_returns_every_row_and_reverses_scores() {
    let path = unique_db_path("sql33-desc-all-rows");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    seed(&core);
    let desc = run(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC LIMIT 100",
    );
    let asc = run(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 100",
    );
    assert_eq!(desc.rows.len(), rows().len());
    assert_eq!(asc.rows.len(), rows().len());
    // 同じ行のスコアは昇順と等しい（並びだけが逆になる）。
    // `ResultRow.score` は演算子の距離値（Issue #1991。`<=>` は `1 - cos`）。NaN も含めビットで比べる。
    for row in &desc.rows {
        let same = asc
            .rows
            .iter()
            .find(|r| r.id == row.id)
            .expect("row in asc");
        assert_eq!(row.score.to_bits(), same.score.to_bits(), "id={}", row.id);
    }
    // 先頭は NaN（id 5 のゼロノルム行）で、残りは距離の降順。
    let pairs: Vec<(u64, f64)> = desc.rows.iter().map(|r| (r.id, r.score)).collect();
    assert_eq!(pairs[0].0, 5);
    assert!(pairs[0].1.is_nan());
    let mut sorted = pairs.clone();
    sorted.sort_by(far_first);
    assert_eq!(
        pairs.iter().map(|p| p.0).collect::<Vec<_>>(),
        sorted.iter().map(|p| p.0).collect::<Vec<_>>(),
        "DESC must be farthest first"
    );
}

// SQL-33 (2): 明示の ASC は方向省略と同じ結果。
#[test]
fn sql33_explicit_asc_matches_default_direction() {
    let path = unique_db_path("sql33-asc-explicit");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    seed(&core);
    let default = run(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 4",
    );
    for dir in ["ASC", "asc"] {
        let explicit = run(
            &core,
            "tenant-a",
            &format!("SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' {dir} LIMIT 4"),
        );
        assert_eq!(ids(&explicit), ids(&default), "{dir}");
    }
}

// SQL-33 (6) と同じテナント境界: 他テナントの行は DESC の結果に 1 件も入らない。
#[test]
fn sql33_desc_never_returns_other_tenants_rows() {
    let path = unique_db_path("sql33-desc-tenant");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    seed(&core);
    // 他テナントの行は問い合わせから最も遠い値（コサイン距離 2。DESC の先頭になりうる）にする。
    seed_tenant(
        &core,
        "tenant-b",
        &[
            (101, [-50.0, 0.0, 0.0], "ja"),
            (102, [-60.0, 0.0, 0.0], "en"),
        ],
    );
    let a = run(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC LIMIT 100",
    );
    assert!(ids(&a).iter().all(|id| *id < 100), "{:?}", ids(&a));
    assert_eq!(a.rows.len(), rows().len());
    let b = run(
        &core,
        "tenant-b",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC LIMIT 100",
    );
    // どちらもコサイン距離 2 の同点なので、順序キー昇順（Issue #1991。内積順では 102 が先だった）。
    assert_eq!(ids(&b), vec![101, 102]);
}

// precision の確信度ゲートは遠い順では意味が定まらないため、併用は 0A000（fail-closed）。
#[test]
fn sql33_desc_with_precision_mode_is_feature_not_supported() {
    let path = unique_db_path("sql33-desc-precision");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    seed(&core);
    let code = run_err_code(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC LIMIT 3 USING MODE 'precision'",
    );
    assert_eq!(code, "0A000");
    // 方向なしの precision は従来どおり受理される。
    run(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3 USING MODE 'precision'",
    );
}

// 構文の拒否は従来どおり 42601（NULLS・重複した方向語・LIMIT 省略・OFFSET）。
#[test]
fn sql33_desc_unsupported_shapes_stay_syntax_errors() {
    let path = unique_db_path("sql33-desc-syntax");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    seed(&core);
    for sql in [
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC NULLS LAST LIMIT 3",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC DESC LIMIT 3",
    ] {
        assert_eq!(run_err_code(&core, "tenant-a", sql), "42601", "{sql}");
    }
}

// まだ SQL から実行できない距離演算子は、DESC を付けても ASC と同じコード（実行段の 0A000）で拒否される。
#[test]
fn sql33_desc_on_not_yet_runnable_operators_fails_like_asc() {
    let path = unique_db_path("sql33-desc-unrunnable");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    seed(&core);
    for op in ["<->", "<+>"] {
        let asc = {
            let mut session = SessionState::default();
            core.execute_sql_in_session(
                &ctx("tenant-a"),
                &mut session,
                &format!("SELECT id FROM documents ORDER BY embedding {op} '[1,0,0]' LIMIT 3"),
            )
            .err()
            .map(|e| e.wire_code().to_string())
        };
        let desc = {
            let mut session = SessionState::default();
            core.execute_sql_in_session(
                &ctx("tenant-a"),
                &mut session,
                &format!("SELECT id FROM documents ORDER BY embedding {op} '[1,0,0]' DESC LIMIT 3"),
            )
            .err()
            .map(|e| e.wire_code().to_string())
        };
        assert_eq!(asc, desc, "{op}");
        assert_eq!(asc.as_deref(), Some("0A000"), "{op}");
    }
}

// 拡張クエリプロトコル: `$1 DESC` が Parse・Bind・Execute を通り、リテラル形と同じ結果を返す。
#[test]
fn sql33_desc_works_through_prepared_parameters() {
    let path = unique_db_path("sql33-desc-prepared");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    seed(&core);
    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents ORDER BY embedding <=> $1 DESC LIMIT 3")
        .expect("parse_sql_prepared");
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
    let literal = run(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC LIMIT 3",
    );
    assert_eq!(ids(&result), ids(&literal));
    let metric = &runnable_metrics()[0];
    assert_eq!(
        ids(&result),
        model_desc(metric, &[1.0, 0.0, 0.0], |_| true, 3)
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

// SQL-75 (4): EXPLAIN は索引を使わない理由（DESC）を出し、索引名注記を付けない。昇順は従来どおり。
#[test]
fn sql75_explain_reports_plain_scan_descending_for_desc_only() {
    for hnsw in [false, true] {
        let path = unique_db_path("sql75-desc-explain");
        let _guard = CleanupGuard(path.clone());
        let core = if hnsw {
            new_hnsw_core(&path)
        } else {
            new_core(&path)
        };
        seed(&core);
        let desc = explain_lines(
            &core,
            "tenant-a",
            "EXPLAIN SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC LIMIT 3",
        );
        assert!(
            desc.iter().any(|l| l == "ann_plan: plain_scan_descending"),
            "hnsw={hnsw}: {desc:?}"
        );
        assert!(!desc.iter().any(|l| l.contains("index=")), "{desc:?}");
        let asc = explain_lines(
            &core,
            "tenant-a",
            "EXPLAIN SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3",
        );
        assert!(
            !asc.iter().any(|l| l.contains("plain_scan_descending")),
            "hnsw={hnsw}: {asc:?}"
        );
        // 明示の ASC は従来の昇順と同じ出力。
        let asc_explicit = explain_lines(
            &core,
            "tenant-a",
            "EXPLAIN SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' ASC LIMIT 3",
        );
        assert_eq!(asc, asc_explicit);
    }
}

// SQL-75 (5): EXPLAIN の出力はテナントによらず同じ（行数などの存在情報を出さない）。
#[test]
fn sql75_explain_desc_is_tenant_independent() {
    let path = unique_db_path("sql75-desc-explain-tenant");
    let _guard = CleanupGuard(path.clone());
    let core = new_hnsw_core(&path);
    seed(&core);
    seed_tenant(&core, "tenant-b", &[(101, [-50.0, 0.0, 0.0], "ja")]);
    let sql = "EXPLAIN SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' DESC LIMIT 3";
    assert_eq!(
        explain_lines(&core, "tenant-a", sql),
        explain_lines(&core, "tenant-b", sql)
    );
    assert_eq!(
        explain_lines(&core, "tenant-a", sql),
        explain_lines(&core, "tenant-c-without-rows", sql)
    );
}
