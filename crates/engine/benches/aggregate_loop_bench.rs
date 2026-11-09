//! 集計の行ループ（可視行ごとの `observe`）の p95 計測ベンチ（Issue #1490。親 #1486・
//! 是正は #1487〜#1489・設計記録は #1491。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-25）。
//!
//! 2026-10 の crossdb 再計測で集計系フェーズだけが退行したが、engine 単体の計測が無く
//! 約 3 週間気づけなかった。本ベンチは 25,000 行 fixture に対し `COUNT(*)`（agg_count）・
//! `COUNT/SUM/AVG/MIN/MAX`（agg_multi）・`COUNT(DISTINCT)` を `execute_sql_in_session`
//! （SQL 表層）経由で測り、`relational_p95_bench` と同じ計測プロトコル・出力形式で出す。
//! 既存の `relational_p95_bench` は GitHub Actions 上の起動を拒否する契約のため変更せず、
//! 本ベンチを別バイナリとして足した（理由は `docs/design/aggregate-loop-bench.md`）。
//!
//! # 出力規約
//!
//! 参考値であり、終了コードに性能の良し悪しを反映しない（非 0 になるのは事前検査の失敗と
//! 計測中の期待外応答だけ）。`.github/workflows/bench.yml` の `bench-aggregate-loop`
//! （workflow_dispatch・週次 schedule）から実行する。共有 runner の値は spec 閾値の確定判定に
//! 使えないため閾値行は常に「未評価」。縮小規模（`BENCH_AGGREGATE_LOOP_ROWS`）は
//! `scale=reduced` と自己ラベルされる。
//!
//! # 安全策
//!
//! fixture には必ず他テナントの Private 行（`lang` が自テナント語彙の外）を混ぜ、計測前に
//! 各 arm を自テナント文脈・他テナント文脈で 1 回ずつ実行して、結果が fixture から独立に導いた
//! 期待値と一致すること（越境があれば COUNT・MAX・DISTINCT が変わる）と、`EXPLAIN` の
//! `access_path:` が想定どおり（行ループを測っていること）を検査する。違反は非 0 終了。
//! 出力にテナント ID・行の値・SQL 全文は含めない。
//!
//! 使い方は `make bench-aggregate-loop`。時間非依存の判定ロジックは `harness::aggregate_loop`
//! にあり `tests/relational_p95_accept.rs` が `make ci` で検証する。

#[allow(dead_code)]
mod harness;

use std::time::Duration;

use harness::aggregate_loop::{
    expected_aggregates, other_rows, parse_rounds, parse_rows,
    refuse_dedicated_under_github_actions, render_derived_line, render_round_line,
    render_summary_line, render_threshold_line, scale_label, Arm, Expected, Viewer, AGG_DIM, ARMS,
    OTHER_TENANT_LANG,
};
use harness::env_report::EnvReport;
use harness::protocol::{run_bounded_retain, MeasurementConfig};
use harness::relational_p95::{lang_for_id, rotate_arms, round_p95, summarize_rounds};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const SEED_BATCH_ROWS: usize = 10_000;
const TENANT_A: &str = "bench-tenant-a";
const TENANT_B: &str = "bench-tenant-b";
const DOCS: &str = "docs";
const WARMUP: u32 = 20;
const MEASURED: u32 = 200;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("aggregate_loop_bench: {msg}");
    std::process::exit(1);
}

fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "n/a".to_string())
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .unwrap_or_else(|e| fail_closed(format!("policy ctx: {e}")))
}

fn docs_schema() -> TableSchema {
    TableSchema::new(
        DOCS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(AGG_DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("qty", ColumnType::BigInt, false),
        ],
    )
}

/// 1 テナント分の行（id 連番・埋め込みは id 依存の固定値）を投入する。
fn seed_rows(
    storage: &Storage,
    schema: &TableSchema,
    tenant_id: &str,
    visibility: Visibility,
    ids: std::ops::Range<u64>,
    lang_of: &dyn Fn(u64) -> String,
) {
    let policy = ctx(tenant_id);
    let mut next = ids.start;
    while next < ids.end {
        let batch_len = SEED_BATCH_ROWS.min((ids.end - next) as usize);
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
        let mut metadata: Vec<Vec<u8>> = Vec::with_capacity(batch_len);
        for i in 0..batch_len {
            let id = next + i as u64;
            let vector: Vec<f32> = (0..AGG_DIM)
                .map(|d| ((id + u64::from(d)) % 7) as f32)
                .collect();
            let values = vec![
                Value::Vector(vector.clone()),
                Value::Text(lang_of(id)),
                Value::BigInt(id as i64),
            ];
            let encoded = encode_scalar_columns(schema, &values)
                .unwrap_or_else(|e| fail_closed(format!("encode row: {e}")));
            vectors.push(vector);
            metadata.push(encoded);
        }
        let rows: Vec<(u64, RowInput<'_>)> = (0..batch_len)
            .map(|i| {
                (
                    next + i as u64,
                    RowInput {
                        tenant_id,
                        visibility,
                        embedding: &vectors[i],
                        metadata: &metadata[i],
                    },
                )
            })
            .collect();
        let op_id = OperationId::parse(&format!("seed-{}-{tenant_id}-{next}", schema.name))
            .unwrap_or_else(|e| fail_closed(format!("operation_id: {e}")));
        tenant::insert_rows(storage, &schema.name, &policy, &rows, &op_id)
            .unwrap_or_else(|e| fail_closed(format!("seed insert: {e}")));
        next += batch_len as u64;
    }
}

fn exec(core: &EngineCore, tenant_ctx: &PolicyContext, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(tenant_ctx, &mut session, sql) {
        Ok(SqlOutcome::Query(result)) => result,
        Ok(_) => fail_closed("statement did not return a query result"),
        Err(e) => fail_closed(format!("statement rejected: {e}")),
    }
}

/// 数値セルを f64 で読む（整数は 2^53 未満の fixture 値のみなので無損失）。
fn cell_num(cell: Option<&Cell>) -> f64 {
    match cell {
        Some(Cell::Integer(v)) => *v as f64,
        Some(Cell::SignedInteger(v)) => *v as f64,
        Some(Cell::Float(v)) => *v,
        // SUM/AVG は NUMERIC（10 進固定小数）で返る。scale 桁ぶん割り戻して f64 にする。
        Some(Cell::Numeric(d)) => d.unscaled() as f64 / 10f64.powi(i32::from(d.scale())),
        other => fail_closed(format!("expected a numeric cell: {other:?}")),
    }
}

/// 結果の 1 行目が期待値と一致することを検査する（越境・空振りを fail-closed で防ぐ）。
fn check_result(arm: &Arm, who: &str, result: &QueryResult, e: &Expected) {
    let Some(row) = result.rows.first() else {
        fail_closed(format!("{}: empty result ({who})", arm.label));
    };
    if result.rows.len() != 1 || row.cells.len() != arm.items {
        fail_closed(format!("{}: unexpected result shape ({who})", arm.label));
    }
    let want: Vec<f64> = match arm.label {
        "agg_count" => vec![e.count as f64],
        "agg_multi" => vec![
            e.count as f64,
            e.sum as f64,
            e.avg,
            e.min as f64,
            e.max as f64,
        ],
        "agg_count_distinct" => vec![e.distinct_lang as f64],
        _ => fail_closed("unknown arm"),
    };
    for (i, w) in want.iter().enumerate() {
        let got = cell_num(row.cells.get(i));
        if (got - w).abs() > 1e-6 {
            fail_closed(format!(
                "{}: unexpected value in cell {i} ({who})",
                arm.label
            ));
        }
    }
}

/// `EXPLAIN` の `access_path:` トークンを取り出す。
fn explain_access_path(core: &EngineCore, tenant_ctx: &PolicyContext, sql: &str) -> String {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(tenant_ctx, &mut session, &format!("EXPLAIN {sql}")) {
        Ok(SqlOutcome::Explain(result)) => result
            .rows
            .iter()
            .filter_map(|r| match r.cells.first() {
                Some(Cell::Text(t)) => t.trim().strip_prefix("access_path: "),
                _ => None,
            })
            .find_map(|t| t.split_whitespace().next().map(str::to_string))
            .unwrap_or_else(|| fail_closed("explain: no access_path line")),
        _ => fail_closed("explain: unexpected outcome"),
    }
}

fn main() {
    let under_gha = std::env::var_os("GITHUB_ACTIONS").is_some();
    let dedicated = std::env::var("BENCH_DEDICATED_ENV")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    if let Err(e) = refuse_dedicated_under_github_actions(under_gha, dedicated) {
        fail_closed(e);
    }
    let rounds = parse_rounds(std::env::var("BENCH_AGGREGATE_LOOP_ROUNDS").ok().as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let rows = parse_rows(std::env::var("BENCH_AGGREGATE_LOOP_ROWS").ok().as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let scale = scale_label(rows);

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "aggregate_loop_bench: rounds={rounds} rows={rows} scale={scale} dedicated_env_attested={dedicated} \
         (informational only; see docs/design/aggregate-loop-bench.md)"
    );

    let path = unique_db_path("issue1490-aggregate-loop");
    let _guard = CleanupGuard(path.clone());
    let storage =
        Storage::open(&path).unwrap_or_else(|e| fail_closed(format!("open storage: {e}")));
    let schema = docs_schema();
    storage
        .create_table(&schema)
        .unwrap_or_else(|e| fail_closed(format!("create table: {e}")));
    let own = rows as u64;
    let other = other_rows(rows) as u64;
    seed_rows(
        &storage,
        &schema,
        TENANT_A,
        Visibility::Public,
        0..own,
        &|id| lang_for_id(id).to_string(),
    );
    // 他テナントの Private 行。lang は自テナント語彙の外で、越境すれば COUNT・MAX・DISTINCT が変わる。
    seed_rows(
        &storage,
        &schema,
        TENANT_B,
        Visibility::Private,
        own..own + other,
        &|_| OTHER_TENANT_LANG.to_string(),
    );
    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    let ctx_a = ctx(TENANT_A);
    let ctx_b = ctx(TENANT_B);

    let exp_a = expected_aggregates(own, other, Viewer::Own)
        .unwrap_or_else(|| fail_closed("expected values overflow"));
    let exp_b = expected_aggregates(own, other, Viewer::Other)
        .unwrap_or_else(|| fail_closed("expected values overflow"));

    // 事前検査 1・2: 自テナント文脈と他テナント文脈で期待値と一致（この呼び出しが温めも兼ねる）。
    for arm in &ARMS {
        check_result(arm, "own", &exec(&core, &ctx_a, arm.sql), &exp_a);
        check_result(arm, "other", &exec(&core, &ctx_b, arm.sql), &exp_b);
    }
    // 事前検査 3: 想定どおりの走査経路（行ループを測っていること）。
    for arm in &ARMS {
        let token = explain_access_path(&core, &ctx_a, arm.sql);
        println!("path arm={} access_path={token}", arm.label);
        if token != arm.access_path {
            fail_closed(format!("{}: unexpected access path", arm.label));
        }
    }

    let config = MeasurementConfig::new(WARMUP, MEASURED, 1)
        .unwrap_or_else(|e| fail_closed(format!("measurement config: {e}")));
    let mut per_arm: Vec<Vec<Duration>> = vec![Vec::new(); ARMS.len()];
    for round in 0..rounds as usize {
        let load = loadavg();
        for idx in rotate_arms(round, ARMS.len()) {
            let Some(arm) = ARMS.get(idx) else { continue };
            // 戻り値（`QueryResult`）の解放を計測区間の外へ出す（`retain_capacity == 0`）。
            let (m, _) = run_bounded_retain(&config, 0, || {
                let mut session = SessionState::default();
                core.execute_sql_in_session(&ctx_a, &mut session, arm.sql)
                    .unwrap_or_else(|e| fail_closed(format!("{}: execute: {e}", arm.label)))
            })
            .unwrap_or_else(|e| fail_closed(format!("{}: protocol violation: {e}", arm.label)));
            let p95 = round_p95(&m.samples).unwrap_or_else(|e| fail_closed(e));
            println!(
                "{}",
                render_round_line(arm.label, round + 1, p95, m.summary.median, &load)
            );
            if let Some(v) = per_arm.get_mut(idx) {
                v.push(p95);
            }
        }
    }
    for (arm, samples) in ARMS.iter().zip(&per_arm) {
        let s = summarize_rounds(samples).unwrap_or_else(|e| fail_closed(e));
        println!("{}", render_summary_line(arm.label, rows, scale, &s));
        println!("{}", render_derived_line(arm, rows, &s));
    }
    println!("{}", render_threshold_line());
}
