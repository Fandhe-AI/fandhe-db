//! 述語（`OR`／`IN`）・スカラー `ORDER BY`・2 テーブル結合の p95 計測ベンチ（Issue #1204。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-24・SQL-25・SQL-28、RLS-10）。
//!
//! これらの形は `EngineCore::execute_sql_in_session`（SQL 表層）の関係演算経路を通るが、
//! 回帰ベンチが無かったため p95 を測る入口として追加する。規模は SQL-24／25／28 の
//! 数値基準の計測規模に合わせる（値は `harness::relational_p95` の定数。spec 本文は転記しない）。
//!
//! # 出力規約
//!
//! 情報提供専用で CI（`.github/workflows/*`）へは配線しない（GitHub ホステッド runner では
//! 起動を拒否する）。共有環境の値は spec 閾値の確定判定に使えない（policy §5・§6）ため、
//! 閾値行は `BENCH_DEDICATED_ENV=1` の申告が無い限り「未評価」と出力する。
//! 縮小規模（`BENCH_RELATIONAL_P95_ROWS`）の run は `scale=reduced` と自己ラベルされ、記録には使わない。
//!
//! # 安全策
//!
//! fixture には必ず他テナントの Private 行を混ぜ、計測前に各 arm を 1 回実行して
//! 「受理される・非空・他テナント行が混入しない・順序／述語／結合キーが期待どおり」を
//! 検査する（RLS を外した状態や空振りを測る事態を fail-closed で防ぐ）。
//! `LIMIT` 後の結果だけでは越境を見逃すため、他テナント行は各 arm の上位に必ず並ぶ
//! sentinel（述語 arm は計測クエリと同一のベクトル・`lang=l0`、順序 arm は最小／最大 `qty`）として投入し、
//! 結合は `MAX_SEARCH_K` 以内の `LIMIT`／`OFFSET` ページ分割で全件取得して件数まで照合する。加えて他テナント文脈で同じ文が
//! 他テナント行を返すこと（＝越境が起きれば見える fixture であること）を確認する。
//!
//! 使い方は `make bench-relational-p95`。時間非依存の判定ロジックは
//! `harness::relational_p95` にあり `tests/relational_p95_accept.rs` が `make ci` で検証する。

#[allow(dead_code)]
mod harness;

use std::time::Duration;

use harness::env_report::EnvReport;
use harness::protocol::{run_bounded_retain, MeasurementConfig};
use harness::relational_p95::{
    author_id_for_doc, interleave_with_reference, is_sorted_by_direction, join_statement,
    lang_for_id, lang_in_first_n, lang_token, order_by_statements, other_tenant_rows, parse_group,
    parse_rounds, parse_rows_scale, predicate_statements, qty_for_id, ratio_vs_reference,
    refuse_under_github_actions, render_round_line, render_summary_line, render_threshold_line,
    round_p95, scale_label, summarize_rounds, Group, JOIN_ROWS, REFERENCE_ARM, WIDE_LIMIT,
};
use harness::rng::DeterministicRng;
use harness::sql_c1::vector_literal;

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

const DIM: usize = 768;
const JOIN_DIM: u32 = 2;
const SEED_BATCH_ROWS: usize = 10_000;
const TENANT_A: &str = "bench-tenant-a";
const TENANT_B: &str = "bench-tenant-b";
const DOCS: &str = "docs";
const DOCUMENTS: &str = "documents";
const AUTHORS: &str = "authors";
/// ラウンドあたりの warmup／計測回数（p95 の分解能を確保するため計測 200 回）。
/// 結合の全件検査のページ幅・最大ページ数（`OFFSET` は `MAX_SEARCH_K` 以内）。
/// 結合右辺の越境検出用 probe 間隔。自テナント文書のうち `id % JOIN_PROBE_MOD == JOIN_PROBE_MOD - 1` の行は
/// 他テナントの作者行（右辺）を結合キーに持つ。右辺の RLS が効く限り内部結合から脱落し、右辺 RLS だけが
/// 外れると結果へ現れる（事前検査で検出できる fixture。RLS-10）。
const JOIN_PROBE_MOD: u64 = 100;
const JOIN_PAGE_ROWS: usize = 5_000;
const JOIN_PAGES: usize = 3;
const WARMUP: u32 = 20;
const MEASURED: u32 = 200;

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("relational_p95_bench: {msg}");
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

/// 1 テナント分の行を `SEED_BATCH_ROWS` 単位のチャンクで投入する。
/// `make_values(id)` が列値（先頭はベクトル列）を返す。
fn seed_rows(
    storage: &Storage,
    schema: &TableSchema,
    tenant_id: &str,
    visibility: Visibility,
    ids: std::ops::Range<u64>,
    make_values: &dyn Fn(u64) -> Vec<Value>,
) {
    let policy = ctx(tenant_id);
    let mut next = ids.start;
    while next < ids.end {
        let batch_len = SEED_BATCH_ROWS.min((ids.end - next) as usize);
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_len);
        let mut metadata: Vec<Vec<u8>> = Vec::with_capacity(batch_len);
        for i in 0..batch_len {
            let values = make_values(next + i as u64);
            let vector = match values.first() {
                Some(Value::Vector(v)) => v.clone(),
                _ => fail_closed("first column must be a vector"),
            };
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

fn docs_schema() -> TableSchema {
    TableSchema::new(
        DOCS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("qty", ColumnType::BigInt, false),
        ],
    )
}

fn documents_schema() -> TableSchema {
    TableSchema::new(
        DOCUMENTS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(JOIN_DIM), false),
            ColumnDef::new("title", ColumnType::Text, false),
            ColumnDef::new("author_id", ColumnType::BigInt, false),
        ],
    )
}

fn authors_schema() -> TableSchema {
    TableSchema::new(
        AUTHORS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(JOIN_DIM), false),
            ColumnDef::new("name", ColumnType::Text, false),
        ],
    )
}

fn open_storage(label: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(&format!("issue1204-relational-p95-{label}"));
    let guard = CleanupGuard(path.clone());
    let storage =
        Storage::open(&path).unwrap_or_else(|e| fail_closed(format!("open storage: {e}")));
    (storage, guard)
}

fn exec(core: &EngineCore, tenant_ctx: &PolicyContext, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(tenant_ctx, &mut session, sql) {
        Ok(SqlOutcome::Query(result)) => result,
        Ok(_) => fail_closed("statement did not return a query result"),
        Err(e) => fail_closed(format!("statement rejected: {e}")),
    }
}

fn cell_i64(cell: Option<&Cell>) -> i64 {
    match cell {
        Some(Cell::SignedInteger(v)) => *v,
        _ => fail_closed("expected a signed integer cell"),
    }
}

fn cell_text(cell: Option<&Cell>) -> &str {
    match cell {
        Some(Cell::Text(s)) => s.as_str(),
        _ => fail_closed("expected a text cell"),
    }
}

/// 述語 arm の事前検査: 非空・他テナント id 非混入・期待 `lang` 集合。
fn precheck_predicate(arm: &str, result: &QueryResult, own_rows: u64) {
    if result.rows.is_empty() {
        fail_closed(format!("{arm}: empty result"));
    }
    let n = match arm {
        "pred_eq" => 1,
        "pred_or2" => 2,
        _ => 8,
    };
    for row in &result.rows {
        if row.id >= own_rows {
            fail_closed(format!("{arm}: row outside own tenant id range"));
        }
        if !lang_in_first_n(lang_for_id(row.id), n) {
            fail_closed(format!("{arm}: row does not satisfy predicate"));
        }
    }
}

/// 順序 arm の事前検査: 非空・他テナント id 非混入・並びの方向。
fn precheck_order(arm: &str, result: &QueryResult, own_rows: u64) {
    if result.rows.is_empty() {
        fail_closed(format!("{arm}: empty result"));
    }
    if result.rows.iter().any(|r| r.id >= own_rows) {
        fail_closed(format!("{arm}: row outside own tenant id range"));
    }
    if arm == "order_single" {
        let qty: Vec<i64> = result
            .rows
            .iter()
            .map(|r| cell_i64(r.cells.get(1)))
            .collect();
        if !is_sorted_by_direction(&qty, false) {
            fail_closed("order_single: result is not ascending");
        }
    } else {
        // lang は文字列昇順、同一 lang 内で qty は降順。
        for pair in result.rows.windows(2) {
            if let [a, b] = pair {
                let (la, lb) = (cell_text(a.cells.get(1)), cell_text(b.cells.get(1)));
                if la > lb {
                    fail_closed("order_multi: lang is not ascending");
                }
                if la == lb && cell_i64(a.cells.get(2)) < cell_i64(b.cells.get(2)) {
                    fail_closed("order_multi: qty is not descending within lang");
                }
            }
        }
    }
}

/// 対照検査: 両 visibility を見える他テナント文脈（Public な自行も見える）で同じ文を実行し、
/// 結果に他テナント側の行（id が自範囲外）が現れることを確認する。RLS が外れれば対象テナントの
/// 結果にも同じ sentinel が現れる、という前提（検出可能な fixture）の裏付けになる。
fn precheck_other_tenant_visible(arm: &str, result: &QueryResult, own_rows: u64) {
    if !result.rows.iter().any(|r| r.id >= own_rows) {
        fail_closed(format!(
            "{arm}: control query returned no other-tenant rows"
        ));
    }
}

/// arm 群を輪番で N ラウンド計測し、要約行を出力する。
fn measure_group(
    group: &str,
    size: (usize, &str),
    rounds: u32,
    core: &EngineCore,
    tenant_ctx: &PolicyContext,
    arms: &[(&'static str, String)],
    with_ratio: bool,
) {
    let (rows, scale) = size;
    let config = MeasurementConfig::new(WARMUP, MEASURED, 1)
        .unwrap_or_else(|e| fail_closed(format!("measurement config: {e}")));
    let mut per_arm: Vec<Vec<Duration>> = vec![Vec::new(); arms.len()];
    let ref_idx = arms.iter().position(|(l, _)| *l == REFERENCE_ARM);
    // 比率あり群は参照 arm を各候補の直前に挟む（policy §3 の baseline/cand1/baseline/cand2 輪番）。
    // 各候補は直前に測った参照 arm の p95 と対にして保持し、時間方向の環境変動を比率へ混入させない。
    let ratio_ref = if with_ratio { ref_idx } else { None };
    let mut paired_ref: Vec<Vec<Duration>> = vec![Vec::new(); arms.len()];
    for round in 0..rounds as usize {
        let load = loadavg();
        let mut last_ref: Option<Duration> = None;
        for idx in interleave_with_reference(round, arms.len(), ratio_ref) {
            let Some((label, sql)) = arms.get(idx) else {
                continue;
            };
            // 戻り値（`QueryResult`）の解放を計測区間の外へ出す（`retain_capacity == 0` は計測直後に drop）。
            let (m, _) = run_bounded_retain(&config, 0, || {
                let mut session = SessionState::default();
                core.execute_sql_in_session(tenant_ctx, &mut session, sql)
                    .unwrap_or_else(|e| fail_closed(format!("{label}: execute: {e}")))
            })
            .unwrap_or_else(|e| fail_closed(format!("{label}: protocol violation: {e}")));
            let p95 = round_p95(&m.samples).unwrap_or_else(|e| fail_closed(e));
            println!(
                "{}",
                render_round_line(group, label, round + 1, p95, m.summary.median, &load)
            );
            if let Some(v) = per_arm.get_mut(idx) {
                v.push(p95);
            }
            if ratio_ref == Some(idx) {
                last_ref = Some(p95);
            } else if let (Some(r), Some(v)) = (last_ref, paired_ref.get_mut(idx)) {
                v.push(r);
            }
        }
    }
    let summaries: Vec<_> = per_arm
        .iter()
        .map(|v| summarize_rounds(v).unwrap_or_else(|e| fail_closed(e)))
        .collect();
    for (i, ((label, _), summary)) in arms.iter().zip(&summaries).enumerate() {
        // 比率は候補 min ÷ 「その候補の直前に測った参照 arm」の min（対応付け集計）。
        let ratio = match (ratio_ref, paired_ref.get(i)) {
            (Some(r), Some(refs)) if i != r && !refs.is_empty() => {
                let ref_min = summarize_rounds(refs)
                    .unwrap_or_else(|e| fail_closed(e))
                    .min;
                Some(ratio_vs_reference(summary.min, ref_min).unwrap_or_else(|e| fail_closed(e)))
            }
            _ => None,
        };
        println!(
            "{}",
            render_summary_line(group, label, rows, scale, summary, ratio)
        );
    }
}

fn run_docs_groups(group: Group, rows: usize, rounds: u32) {
    let (storage, _guard) = open_storage("docs");
    let schema = docs_schema();
    storage
        .create_table(&schema)
        .unwrap_or_else(|e| fail_closed(format!("create table: {e}")));
    let make = |id: u64| {
        vec![
            Value::Vector(rng_vector_for(id)),
            Value::Text(lang_for_id(id).to_string()),
            Value::BigInt(qty_for_id(id)),
        ]
    };
    let own = rows as u64;
    let other = other_tenant_rows(rows) as u64;
    let query = DeterministicRng::new(2).next_vector(DIM);
    // 他テナント行は各 arm の上位（`l0`・最小／最大 `qty`）に並ぶ sentinel とする。ベクトルは
    // 計測クエリのベクトルと同一（コサイン距離 0）にして、RLS が外れれば計測クエリ自身の
    // LIMIT 内へ必ず越境行が入る fixture にする。これで対象テナントの事前検査（他テナント id 非混入）が
    // 計測クエリそのものの分離検出力を持つ。対照検査も同じ文を他テナント文脈で実行し、sentinel が
    // 上位に見えること（＝検出可能な fixture であること）を確認する。
    // `qty` は偶数 id を最小（`order_single` 先頭）、奇数 id を最大（`order_multi` の `l0` 内先頭）にする。
    let sentinel_vector: Vec<f32> = query.clone();
    let make_other = |id: u64| {
        vec![
            Value::Vector(sentinel_vector.clone()),
            Value::Text(lang_token(0).to_string()),
            Value::BigInt(if id.is_multiple_of(2) { -1 } else { 2_000_000 }),
        ]
    };
    seed_rows(
        &storage,
        &schema,
        TENANT_A,
        Visibility::Public,
        0..own,
        &make,
    );
    seed_rows(
        &storage,
        &schema,
        TENANT_B,
        Visibility::Private,
        own..own + other,
        &make_other,
    );

    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    let ctx_a =
        PolicyContext::new(TENANT_A).unwrap_or_else(|e| fail_closed(format!("policy ctx: {e}")));
    let ctx_b = ctx(TENANT_B);
    let literal =
        vector_literal(&query).unwrap_or_else(|e| fail_closed(format!("vector literal: {e}")));

    if group.includes(Group::Predicate) {
        let arms = predicate_statements(DOCS, &literal).unwrap_or_else(|e| fail_closed(e));
        for (label, sql) in &arms {
            precheck_predicate(label, &exec(&core, &ctx_a, sql), own);
            // 対照: 同じ計測クエリを他テナント文脈で実行し、sentinel が結果に現れること。
            precheck_other_tenant_visible(label, &exec(&core, &ctx_b, sql), own);
        }
        measure_group(
            "predicate",
            (rows, scale_label(rows)),
            rounds,
            &core,
            &ctx_a,
            &arms,
            true,
        );
    }
    if group.includes(Group::OrderBy) {
        let arms = order_by_statements(DOCS).unwrap_or_else(|e| fail_closed(e));
        for (label, sql) in &arms {
            precheck_order(label, &exec(&core, &ctx_a, sql), own);
            precheck_other_tenant_visible(label, &exec(&core, &ctx_b, sql), own);
        }
        measure_group(
            "order_by",
            (rows, scale_label(rows)),
            rounds,
            &core,
            &ctx_a,
            &arms,
            false,
        );
    }
}

/// id から決定的にベクトルを作る（行ごとに `DeterministicRng` を seed する）。
fn rng_vector_for(id: u64) -> Vec<f32> {
    DeterministicRng::new(id.wrapping_add(1_000)).next_vector(DIM)
}

/// 結合文を `LIMIT`／`OFFSET` のページ（各 `JOIN_PAGE_ROWS` 行）で全件取得し、行 id を返す。
/// `LIMIT`・`OFFSET` とも `MAX_SEARCH_K` 以内に収めるための分割で、末尾ページが満杯のまま
/// 取得上限に達した場合は全件取得できていないため fail-closed する。
fn exec_join_pages(core: &EngineCore, tenant_ctx: &PolicyContext, head: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    for page in 0..JOIN_PAGES {
        let sql = format!(
            "{head}LIMIT {JOIN_PAGE_ROWS} OFFSET {}",
            page * JOIN_PAGE_ROWS
        );
        let result = exec(core, tenant_ctx, &sql);
        let len = result.rows.len();
        ids.extend(result.rows.iter().map(|r| r.id));
        if len < JOIN_PAGE_ROWS {
            return ids;
        }
    }
    fail_closed("join_inner: full join exceeds the paging capacity")
}

fn run_join_group(rows: usize, rounds: u32) {
    let join_rows = rows.min(JOIN_ROWS) as u64;
    let (storage, _guard) = open_storage("join");
    let documents = documents_schema();
    let authors = authors_schema();
    for schema in [&documents, &authors] {
        storage
            .create_table(schema)
            .unwrap_or_else(|e| fail_closed(format!("create table: {e}")));
    }
    let other = other_tenant_rows(join_rows as usize) as u64;
    let is_probe = |id: u64| id % JOIN_PROBE_MOD == JOIN_PROBE_MOD - 1;
    let make_doc = |id: u64| {
        // probe 行は他テナントの作者行（id `join_rows..`）を指す。右辺 RLS が効けば内部結合で脱落する。
        let author = if is_probe(id) {
            (join_rows + id % other.max(1)) as i64
        } else {
            author_id_for_doc(id, join_rows)
        };
        vec![
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(format!("t{id}")),
            Value::BigInt(author),
        ]
    };
    // 他テナント文書は他テナントの作者行（id `join_rows..`）へ結合させ、対照クエリで結合が成立するようにする。
    let make_other_doc = |id: u64| {
        vec![
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(format!("t{id}")),
            Value::BigInt((join_rows + (id - join_rows) % other.max(1)) as i64),
        ]
    };
    let make_author = |id: u64| {
        vec![
            Value::Vector(vec![id as f32, 1.0]),
            Value::Text(format!("a{id}")),
        ]
    };
    seed_rows(
        &storage,
        &documents,
        TENANT_A,
        Visibility::Public,
        0..join_rows,
        &make_doc,
    );
    seed_rows(
        &storage,
        &documents,
        TENANT_B,
        Visibility::Private,
        join_rows..join_rows + other,
        &make_other_doc,
    );
    seed_rows(
        &storage,
        &authors,
        TENANT_A,
        Visibility::Public,
        0..join_rows,
        &make_author,
    );
    seed_rows(
        &storage,
        &authors,
        TENANT_B,
        Visibility::Private,
        join_rows..join_rows + other,
        &make_author,
    );

    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    let ctx_a =
        PolicyContext::new(TENANT_A).unwrap_or_else(|e| fail_closed(format!("policy ctx: {e}")));
    let arm = join_statement(DOCUMENTS, AUTHORS).unwrap_or_else(|e| fail_closed(e));
    let result = exec(&core, &ctx_a, &arm.1);
    if result.rows.is_empty() {
        fail_closed("join_inner: empty result");
    }
    // 結合キー一致: 文書 id の作者は `a{author_id}`、タイトルは `t{id}`（doc id = row id）。
    for row in &result.rows {
        if row.id >= join_rows {
            fail_closed("join_inner: row outside own tenant id range");
        }
        if is_probe(row.id) {
            fail_closed("join_inner: right-side tenant boundary leaked (probe row joined)");
        }
        let title = cell_text(row.cells.first());
        let name = cell_text(row.cells.get(1));
        let expected_author = author_id_for_doc(row.id, join_rows);
        if title != format!("t{}", row.id) || name != format!("a{expected_author}") {
            fail_closed("join_inner: join key mismatch");
        }
    }
    // 対照検査: 結合結果を `MAX_SEARCH_K` 以内の `LIMIT`／`OFFSET` ページで全件取得し、対象テナントの
    // 結合結果が自テナント文書の全件（過不足なし）であることを確認する。RLS が外れれば他テナント
    // 文書が加わり件数・id 範囲が崩れる。他テナント文脈では自行と Public な他テナント行の双方が見える。
    let head = arm
        .1
        .strip_suffix(&format!("LIMIT {WIDE_LIMIT}"))
        .unwrap_or_else(|| fail_closed("join_inner: unexpected statement shape"));
    let full = exec_join_pages(&core, &ctx_a, head);
    let own_joinable = (0..join_rows).filter(|id| !is_probe(*id)).count() as u64;
    if own_joinable >= join_rows {
        fail_closed("join_inner: fixture has no cross-tenant probe rows (scale too small)");
    }
    if full.len() as u64 != own_joinable || full.iter().any(|id| *id >= join_rows || is_probe(*id))
    {
        fail_closed("join_inner: full join is not confined to own tenant");
    }
    let ctx_b = ctx(TENANT_B);
    let full_b = exec_join_pages(&core, &ctx_b, head);
    if full_b.len() as u64 != join_rows + other
        || !full_b.iter().any(|id| *id >= join_rows)
        || !full_b.iter().any(|id| *id < join_rows)
    {
        fail_closed("join_inner: control query did not see both tenants' rows");
    }
    measure_group(
        "join",
        (join_rows as usize, scale_label(rows)),
        rounds,
        &core,
        &ctx_a,
        &[arm],
        false,
    );
}

fn main() {
    if let Err(e) = refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some()) {
        fail_closed(e);
    }
    let rounds = parse_rounds(std::env::var("BENCH_RELATIONAL_P95_ROUNDS").ok().as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let group = parse_group(std::env::var("BENCH_RELATIONAL_P95_GROUP").ok().as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let rows = parse_rows_scale(std::env::var("BENCH_RELATIONAL_P95_ROWS").ok().as_deref())
        .unwrap_or_else(|e| fail_closed(e));
    let dedicated = std::env::var("BENCH_DEDICATED_ENV")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", engine::isa::current().isa()))
    );
    println!(
        "relational_p95_bench: group={} rounds={rounds} rows={rows} dedicated_env_attested={dedicated} \
         (informational only; see docs/design/relational-p95-bench.md)",
        group.label()
    );

    if group.includes(Group::Predicate) || group.includes(Group::OrderBy) {
        run_docs_groups(group, rows, rounds);
    }
    if group.includes(Group::Join) {
        run_join_group(rows, rounds);
    }
    println!("{}", render_threshold_line(dedicated));
}
