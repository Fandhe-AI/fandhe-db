//! 衝突有無で応答時間分布を比較するベンチハーネス（Issue #2079。親 #2078。
//! ポインタ: `docs/spec/04-behavior/` RLS-9・RLS-10）。
//!
//! 他テナントにだけ UNIQUE 値・FK 親がある状態（flooded）と、どこにも無い状態（baseline）で、
//! 同一の書き込み文（UNIQUE 違反・FK 親不在・正常）を繰り返し実行し、応答時間の生サンプルと
//! p50／p95／p99 を JSON で出力する。応答内容の不変性は `tests/rls10_write_constraint_paths.rs`
//! の T4 が固定しており、本ベンチはその対象外だったレイテンシ分布の計測入口である。
//! seed は同テストと同じ方式（従来方式の表 DDL → `insert_typed_row` → テンプレート DB の
//! ファイル複製）を流用する。
//!
//! # 責務の境界
//!
//! 判定基準の設定・実測値の記録・CI 配線は Issue #2080 の担当で、本ベンチは pass/fail を判定しない
//! （情報提供専用。`.github/workflows/*` へは配線しない）。共有環境の値は採否の根拠にしない
//! （`docs/design/benchmark-judgement-policy.md` §5）。
//!
//! # 安全策（fail-closed）
//!
//! 計測前に各形状を baseline／flooded の使い捨て複製で 1 回ずつ実行し、期待どおりの応答であること、
//! 2 arm の応答（`wire_code`・`client_message`・影響行数）が一致すること、flooded に他テナント行が
//! 実在し baseline には無いことを物理走査で確かめる。1 件でも外れたら非 0 終了する。
//!
//! 使い方は `make bench-rls-timing`（env と出力の詳細は `docs/design/rls-timing-bench.md`）。
//! 時間非依存のロジックは `harness::rls_timing` にあり `tests/rls_timing_accept.rs` が検証する。

#[allow(dead_code)]
mod harness;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use harness::env_report::EnvReport;
use harness::protocol::{run_bounded_retain, MeasurementConfig};
use harness::relational_p95::rotate_arms;
use harness::rls_timing::{
    build_statements, check_env_consistency, classify_matches, common_rows, flood_extra_rows,
    parse_iters, parse_rounds, parse_shapes, render_json, render_summary_line, summarize, Arm,
    Cell, EnvInfo, Outcome, Round, SeedCol, SeedRow, Shape, ALL_ARMS, DDL, NORMAL_ID_BASE, SEED,
    TENANT_FLOODERS, TENANT_VIEWER, WARMUP,
};

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLES: [&str; 3] = ["uniq", "parents", "children"];

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("rls_timing_bench: {msg}");
    std::process::exit(1);
}

fn workspace_root() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    Path::new(&manifest_dir).join("../..")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .unwrap_or_else(|e| fail_closed(format!("policy ctx: {e}")))
}

fn to_value(c: &SeedCol) -> Value {
    match c {
        SeedCol::Text(s) => Value::Text(s.clone()),
        SeedCol::BigInt(n) => Value::BigInt(*n),
    }
}

/// 従来方式の表 DDL → seed 投入済みのテンプレート DB を作る（`flood` なら他 2 テナントの行を足す）。
/// ここで使うパスは呼び出し元の `CleanupGuard` が削除する。
fn build_template(flood: bool, n_normal: u32) -> (PathBuf, CleanupGuard) {
    let path = unique_db_path("rls-timing-template");
    let guard = CleanupGuard(path.clone());
    {
        let core = EngineCore::from_storage(
            Storage::open(&path).unwrap_or_else(|e| fail_closed(format!("open template: {e:?}"))),
            Box::new(CpuScalarProvider),
        );
        let sys = ctx("sys");
        let mut session = SessionState::default();
        session.allow_ddl();
        for ddl in DDL {
            if let Err(e) = core.execute_sql_in_session_legacy(&sys, &mut session, ddl) {
                fail_closed(format!("ddl failed: {}", e.wire_code()));
            }
        }
    }
    let storage =
        Storage::open(&path).unwrap_or_else(|e| fail_closed(format!("reopen template: {e:?}")));
    let mut tenants: Vec<(&str, Vec<SeedRow>)> = vec![(TENANT_VIEWER, common_rows(TENANT_VIEWER))];
    if flood {
        for t in TENANT_FLOODERS {
            let mut rows = common_rows(t);
            rows.extend(flood_extra_rows(t, n_normal));
            tenants.push((t, rows));
        }
    }
    for (tenant, rows) in tenants {
        let policy = ctx(tenant);
        for r in rows {
            let op = OperationId::parse(&format!("seed-{}-{tenant}-{}", r.table, r.id))
                .unwrap_or_else(|_| fail_closed("seed operation id"));
            let values: Vec<Value> = r.cols.iter().map(to_value).collect();
            if let Err(e) = engine::tenant::insert_typed_row(
                &storage,
                r.table,
                &policy,
                r.id,
                Visibility::Public,
                &values,
                &op,
            ) {
                fail_closed(format!("seed {} id={} failed: {e:?}", r.table, r.id));
            }
        }
    }
    (path, guard)
}

/// 物理走査（RLS を介さない）で `table` の `(tenant_id, id)` を全件返す。
fn physical_rows(path: &Path, table: &str) -> Vec<(String, u64)> {
    let storage =
        Storage::open(path).unwrap_or_else(|e| fail_closed(format!("open for scan: {e:?}")));
    let mut out = Vec::new();
    let mut after: Option<(String, u64)> = None;
    loop {
        let (page, next) = storage
            .scan_table_page(
                table,
                after.as_ref().map(|(t, id)| (t.as_str(), *id)),
                10_000,
            )
            .unwrap_or_else(|e| fail_closed(format!("scan_table_page: {e:?}")));
        out.extend(page.into_iter().map(|r| (r.tenant_id, r.id)));
        match next {
            Some(c) => after = Some(c),
            None => break,
        }
    }
    out
}

fn normalize(res: Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError>) -> Outcome {
    match res {
        Ok(SqlOutcome::Insert(x)) => Outcome::Affected(x.rows_affected),
        Ok(_) => Outcome::Other,
        Err(e) => Outcome::Rejected {
            code: e.wire_code().to_string(),
            message: e.client_message(),
        },
    }
}

/// テンプレートを新しい一意パスへ複製して開く（複製は計測区間の外）。
fn open_copy(template: &Path) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("rls-timing-run");
    let guard = CleanupGuard(path.clone());
    if let Err(e) = std::fs::copy(template, &path) {
        fail_closed(format!("copy template: {e}"));
    }
    let core = EngineCore::from_storage(
        Storage::open(&path).unwrap_or_else(|e| fail_closed(format!("open copy: {e:?}"))),
        Box::new(CpuScalarProvider),
    );
    (core, guard)
}

/// 事前検査: flooded に他テナント行が実在し、baseline には無いこと。
fn check_fixtures(baseline: &Path, flooded: &Path, shapes: &[Shape]) {
    for table in TABLES {
        if physical_rows(baseline, table)
            .iter()
            .any(|(t, _)| t != TENANT_VIEWER)
        {
            fail_closed(format!("baseline fixture has foreign rows in {table}"));
        }
    }
    let has = |table: &str, id: u64| {
        let rows = physical_rows(flooded, table);
        TENANT_FLOODERS
            .iter()
            .all(|f| rows.iter().any(|(t, i)| t == f && *i == id))
    };
    if !has("uniq", 1) {
        fail_closed("flooded fixture lacks the colliding UNIQUE row");
    }
    if shapes.contains(&Shape::FkMissingParent) && !has("parents", 7) {
        fail_closed("flooded fixture lacks the foreign-only FK parent");
    }
    if shapes.contains(&Shape::Normal) && !has("uniq", NORMAL_ID_BASE) {
        fail_closed("flooded fixture lacks the colliding normal row");
    }
}

/// 事前検査: 形状ごとに 1 文を各 arm の使い捨て複製で実行し、期待どおり・両 arm 一致を確かめる。
fn precheck_shape(shape: Shape, baseline: &Path, flooded: &Path) {
    let sql = build_statements(0, shape, 1).remove(0);
    let policy = ctx(TENANT_VIEWER);
    let mut seen: Vec<Outcome> = Vec::new();
    for template in [baseline, flooded] {
        let (core, _guard) = open_copy(template);
        let mut session = SessionState::default();
        let outcome = normalize(core.execute_sql_in_session(&policy, &mut session, &sql));
        if !classify_matches(shape.expected(), &outcome) {
            fail_closed(format!(
                "precheck: unexpected outcome for shape={}",
                shape.token()
            ));
        }
        seen.push(outcome);
    }
    if seen.first() != seen.get(1) {
        fail_closed(format!(
            "precheck: baseline and flooded responses differ for shape={} (RLS-10 violation)",
            shape.token()
        ));
    }
    println!("rls_timing precheck shape={} ok", shape.token());
}

/// 1 セル（round, shape, arm）を計測する。
fn run_cell(round: u32, shape: Shape, arm: Arm, template: &Path, iters: u32) -> Cell {
    let total = WARMUP + iters;
    // SQL の生成は計測前に済ませる（計測区間に format! を入れない）。
    let statements = build_statements(round, shape, total);
    let (core, _guard) = open_copy(template);
    let policy = ctx(TENANT_VIEWER);
    let mut session = SessionState::default();
    let expected = shape.expected();
    let cfg = MeasurementConfig::new(WARMUP, iters, SEED)
        .unwrap_or_else(|e| fail_closed(format!("measurement config: {e:?}")));
    let mut next = 0usize;
    let mut mismatches = 0u32;
    let (m, _) = run_bounded_retain(&cfg, 0, || {
        let sql = statements.get(next).map(String::as_str).unwrap_or("");
        next += 1;
        let res = core.execute_sql_in_session(&policy, &mut session, sql);
        let ok = match (&res, expected) {
            (Err(e), harness::rls_timing::Expected::Rejected(c)) => e.wire_code() == c,
            (Ok(SqlOutcome::Insert(x)), harness::rls_timing::Expected::Affected(n)) => {
                x.rows_affected == n
            }
            _ => false,
        };
        if !ok {
            mismatches += 1;
        }
        res
    })
    .unwrap_or_else(|e| fail_closed(format!("measurement failed: {e:?}")));
    Cell {
        shape,
        arm,
        mismatches,
        samples: m.samples,
    }
}

fn git_commit() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace_root())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn main() {
    let get = |k: &str| std::env::var(k).ok();
    let rounds_n =
        parse_rounds(get("BENCH_RLS_TIMING_ROUNDS").as_deref()).unwrap_or_else(|e| fail_closed(e));
    let iters =
        parse_iters(get("BENCH_RLS_TIMING_ITERS").as_deref()).unwrap_or_else(|e| fail_closed(e));
    let shapes =
        parse_shapes(get("BENCH_RLS_TIMING_SHAPES").as_deref()).unwrap_or_else(|e| fail_closed(e));
    let dedicated = get("BENCH_DEDICATED_ENV").as_deref() == Some("1");
    let github_actions = get("GITHUB_ACTIONS").is_some();
    check_env_consistency(github_actions, dedicated).unwrap_or_else(|e| fail_closed(e));

    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out_dir_raw = get("BENCH_RLS_TIMING_OUT_DIR")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("target/bench-rls-timing/{unix_secs}"));
    let out_dir = workspace_root().join(&out_dir_raw);
    let raw_json = out_dir.join("raw.json");
    if raw_json.exists() {
        fail_closed(format!(
            "refusing to overwrite existing {out_dir_raw}/raw.json"
        ));
    }

    let env = EnvReport::capture(format!("{:?}", engine::isa::current().isa()));
    println!("{env}");
    if !dedicated {
        println!(
            "rls_timing_bench: BENCH_DEDICATED_ENV not set; treat results as shared-environment \
             reference values only, not as an adoption basis (benchmark-judgement-policy.md §5)"
        );
    }

    let n_normal = if shapes.contains(&Shape::Normal) {
        WARMUP + iters
    } else {
        0
    };
    let (baseline, _g_base) = build_template(false, 0);
    let (flooded, _g_flood) = build_template(true, n_normal);
    check_fixtures(&baseline, &flooded, &shapes);
    for &shape in &shapes {
        precheck_shape(shape, &baseline, &flooded);
    }

    let mut rounds: Vec<Round> = Vec::new();
    for r in 0..rounds_n {
        let mut cells = Vec::new();
        for si in rotate_arms(r as usize, shapes.len()) {
            let Some(&shape) = shapes.get(si) else {
                continue;
            };
            for ai in rotate_arms(r as usize, ALL_ARMS.len()) {
                let Some(&arm) = ALL_ARMS.get(ai) else {
                    continue;
                };
                let template = match arm {
                    Arm::Baseline => &baseline,
                    Arm::Flooded => &flooded,
                };
                cells.push(run_cell(r, shape, arm, template, iters));
            }
        }
        rounds.push(Round { index: r, cells });
    }

    let mismatches: u32 = rounds
        .iter()
        .flat_map(|r| r.cells.iter())
        .map(|c| c.mismatches)
        .sum();
    if mismatches > 0 {
        fail_closed(format!(
            "{mismatches} measured responses did not match the expected outcome"
        ));
    }

    let summary = summarize(&rounds, &shapes).unwrap_or_else(|e| fail_closed(e));
    for row in &summary {
        println!(
            "{}",
            render_summary_line("pooled", row.shape, row.arm, &row.pooled)
        );
        for (ri, q) in &row.per_round {
            println!(
                "{}",
                render_summary_line(&format!("round={ri}"), row.shape, row.arm, q)
            );
        }
    }

    let info = EnvInfo {
        os: env.os.to_string(),
        arch: env.arch.to_string(),
        logical_cpus: env.logical_cpus,
        isa: env.isa.clone(),
        loadavg: env.loadavg.clone(),
        dedicated_env_attested: dedicated,
        github_actions,
        git_commit: git_commit(),
    };
    let json = render_json(&info, iters, &shapes, &rounds).unwrap_or_else(|e| fail_closed(e));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        fail_closed(format!("failed to create {out_dir_raw}: {e}"));
    }
    if let Err(e) = std::fs::write(&raw_json, json) {
        fail_closed(format!("failed to write {out_dir_raw}/raw.json: {e}"));
    }
    println!("rls_timing_bench: wrote {out_dir_raw}/raw.json");
}
