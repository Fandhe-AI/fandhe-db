//! 変更ログ（RECOVER-14 のポインタ。Issue #3069。設計は `docs/design/change-log-pitr.md` の D9・D11）の
//! 有効時と無効時のコストを測る情報提供専用ベンチ。spec 由来の pass/fail 閾値は持たず、
//! `.github/workflows/*` へは配線しない（手元・専有環境での実測用。`benchmark-judgement-policy.md` に従い、
//! 無効と有効を交互に N ペア実行して中央値とばらつきを併記する）。
//!
//! 実行: `cargo bench -p fandhe-db-engine --bench change_log_bench -- <mode> [key=value ...]`
//!
//! | mode | 測るもの | 主なパラメータ（既定） |
//! | ---- | -------- | ---------------------- |
//! | `single` | 1 行 commit（`tenant::insert_rows` 1 行）の遅延 | `n=2000` `pairs=5` |
//! | `bulk` | 大きなバッチの ingest（1 commit = `rows` 行） | `rows=1000` `dim=128` `batches=20` `pairs=5` |
//! | `tables` | 表が `tables` 個ある DB での 1 行 commit の遅延（層 2 の表数依存） | `tables=1000` `n=300` `pairs=5` |
//! | `limits` | 1 commit に載せる行数を指定して 1 回投入し、frame の上限で拒否される境界とメモリの最大値を見る（行数ごとに別プロセスで呼ぶ） | `rows=100000` `dim=128` `log=1` |
//!
//! 公開 API（`Storage`・`tenant::insert_rows`）だけを使い、素の redb は開かない。

use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "tenant-clog";

fn schema(name: &str, dim: usize) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(dim as u32), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// 決定的な擬似乱数（依存を増やさない簡易 xorshift）。
struct Xor(u64);
impl Xor {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| (self.next() % 10_000) as f32 / 10_000.0)
            .collect()
    }
}

fn open(label: &str, log: bool) -> (CleanupGuard, Storage, std::path::PathBuf) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    if log {
        storage.enable_change_log().expect("enable change log");
    }
    (guard, storage, path)
}

struct Batch {
    ids: Vec<u64>,
    embeddings: Vec<Vec<f32>>,
    metadata: Vec<Vec<u8>>,
}

fn make_batch(sch: &TableSchema, rng: &mut Xor, start_id: u64, rows: usize, dim: usize) -> Batch {
    let mut b = Batch {
        ids: Vec::with_capacity(rows),
        embeddings: Vec::with_capacity(rows),
        metadata: Vec::with_capacity(rows),
    };
    for i in 0..rows {
        let id = start_id + i as u64;
        let e = rng.vector(dim);
        let md = encode_scalar_columns(
            sch,
            &[
                Value::Vector(e.clone()),
                Value::Text(format!("change log bench row {id}")),
            ],
        )
        .expect("encode");
        b.ids.push(id);
        b.embeddings.push(e);
        b.metadata.push(md);
    }
    b
}

fn insert(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    b: &Batch,
    op: &str,
) -> Result<Duration, String> {
    let inputs: Vec<(u64, RowInput<'_>)> = b
        .ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                *id,
                RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility: Visibility::Public,
                    embedding: &b.embeddings[i],
                    metadata: &b.metadata[i],
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(op).expect("operation id");
    let t = Instant::now();
    tenant::insert_rows(storage, table, ctx, &inputs, &op_id).map_err(|e| format!("{e:?}"))?;
    Ok(t.elapsed())
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    v[v.len() / 2]
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[idx.min(v.len() - 1)]
}

fn arg(args: &[String], key: &str, default: usize) -> usize {
    args.iter()
        .find_map(|a| a.strip_prefix(&format!("{key}=")))
        .map(|v| v.parse().expect("integer parameter"))
        .unwrap_or(default)
}

fn file_mib(path: &std::path::Path) -> f64 {
    std::fs::metadata(path).map_or(0.0, |m| m.len() as f64 / (1024.0 * 1024.0))
}

/// 無効と有効を交互に `pairs` 回走らせ、各回の値（ms か µs）を返す。
fn alternate(pairs: usize, mut run: impl FnMut(bool) -> f64) -> (Vec<f64>, Vec<f64>) {
    let (mut off, mut on) = (Vec::new(), Vec::new());
    for i in 0..pairs {
        // 順序の偏りを避けるため、ペアごとに先攻を入れ替える。
        if i % 2 == 0 {
            off.push(run(false));
            on.push(run(true));
        } else {
            on.push(run(true));
            off.push(run(false));
        }
    }
    (off, on)
}

fn report(label: &str, unit: &str, off: &mut [f64], on: &mut [f64]) {
    let (mo, mn) = (median(off), median(on));
    let band = |v: &[f64]| {
        let (lo, hi) = v
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
        format!("{lo:.3}..{hi:.3}")
    };
    println!(
        "{label}: off median={mo:.3}{unit} [{}]  on median={mn:.3}{unit} [{}]  ratio={:.2}x",
        band(off),
        band(on),
        mn / mo
    );
}

fn single(args: &[String]) {
    let n = arg(args, "n", 2000);
    let pairs = arg(args, "pairs", 5);
    let dim = 16;
    let ctx = PolicyContext::new(TENANT).expect("tenant");
    let sch = schema("docs", dim);
    let (mut p50_off, mut p50_on, mut p95_off, mut p95_on) = (vec![], vec![], vec![], vec![]);
    let _ = alternate(pairs, |log| {
        let (_g, storage, _p) = open("clog-single", log);
        storage.create_table(&sch).expect("create");
        let mut rng = Xor(0x9E37_79B9_7F4A_7C15);
        let mut lat = Vec::with_capacity(n);
        for i in 0..n as u64 {
            let b = make_batch(&sch, &mut rng, i + 1, 1, dim);
            let d = insert(&storage, "docs", &ctx, &b, &format!("op-{i}")).expect("insert");
            lat.push(d.as_secs_f64() * 1e6);
        }
        let (p50, p95) = (pct(&mut lat.clone(), 0.5), pct(&mut lat, 0.95));
        if log {
            p50_on.push(p50);
            p95_on.push(p95);
        } else {
            p50_off.push(p50);
            p95_off.push(p95);
        }
        p50
    });
    report("single-row commit p50", "us", &mut p50_off, &mut p50_on);
    report("single-row commit p95", "us", &mut p95_off, &mut p95_on);
}

fn bulk(args: &[String]) {
    let rows = arg(args, "rows", 1000);
    let dim = arg(args, "dim", 128);
    let batches = arg(args, "batches", 20);
    let pairs = arg(args, "pairs", 5);
    let ctx = PolicyContext::new(TENANT).expect("tenant");
    let sch = schema("docs", dim);
    let (mut size_off, mut size_on) = (vec![], vec![]);
    let (mut off, mut on) = alternate(pairs, |log| {
        let (_g, storage, path) = open("clog-bulk", log);
        storage.create_table(&sch).expect("create");
        let mut rng = Xor(0xD1B5_4A32_D192_ED03);
        let prepared: Vec<Batch> = (0..batches as u64)
            .map(|b| make_batch(&sch, &mut rng, b * rows as u64 + 1, rows, dim))
            .collect();
        let t = Instant::now();
        for (i, b) in prepared.iter().enumerate() {
            insert(&storage, "docs", &ctx, b, &format!("bulk-{i}")).expect("insert");
        }
        let secs = t.elapsed().as_secs_f64();
        let mib = file_mib(&path);
        if log {
            size_on.push(mib);
        } else {
            size_off.push(mib);
        }
        (rows * batches) as f64 / secs
    });
    println!("bulk: rows/commit={rows} dim={dim} commits={batches}");
    report("bulk ingest throughput", " rows/s", &mut off, &mut on);
    report("bulk db file size", " MiB", &mut size_off, &mut size_on);
}

fn tables(args: &[String]) {
    let t_count = arg(args, "tables", 1000);
    let n = arg(args, "n", 300);
    let pairs = arg(args, "pairs", 5);
    let dim = 8;
    let ctx = PolicyContext::new(TENANT).expect("tenant");
    let (mut off, mut on) = alternate(pairs, |log| {
        let (_g, storage, _p) = open("clog-tables", log);
        for t in 0..t_count {
            storage
                .create_table(&schema(&format!("t{t}"), dim))
                .expect("create");
        }
        let sch = schema("t0", dim);
        let mut rng = Xor(0xA076_1D64_78BD_642F);
        let mut lat = Vec::with_capacity(n);
        for i in 0..n as u64 {
            let b = make_batch(&sch, &mut rng, i + 1, 1, dim);
            let d = insert(&storage, "t0", &ctx, &b, &format!("tb-{i}")).expect("insert");
            lat.push(d.as_secs_f64() * 1e3);
        }
        median(&mut lat)
    });
    println!("tables: tables={t_count} (1-row commit into one of them)");
    report("1-row commit median", "ms", &mut off, &mut on);
}

/// プロセスのメモリ使用量の最大値（`/proc/self/status` の `VmHWM`。Linux 以外は 0）。
fn peak_rss_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:").map(str::to_string))
        })
        .and_then(|l| l.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
        .map_or(0.0, |kb| kb / 1024.0)
}

/// 1 commit に載せる行数を `rows` にして 1 回だけ投入する（`log=0|1`）。メモリの最大値は
/// プロセス単位なので、境界を探すときは行数ごとに別プロセスで呼ぶ。
fn limits(args: &[String]) {
    let dim = arg(args, "dim", 128);
    let rows = arg(args, "rows", 100_000);
    let log = arg(args, "log", 1) == 1;
    let ctx = PolicyContext::new(TENANT).expect("tenant");
    let sch = schema("docs", dim);
    let (_g, storage, path) = open("clog-limits", log);
    storage.create_table(&sch).expect("create");
    let mut rng = Xor(0xE703_7ED1_A0B4_28DB);
    let b = make_batch(&sch, &mut rng, 1, rows, dim);
    let before = peak_rss_mib();
    let outcome = insert(&storage, "docs", &ctx, &b, "limit-0");
    let payload_estimate_mib = b
        .embeddings
        .iter()
        .zip(&b.metadata)
        .map(|(e, m)| e.len() * 4 + m.len() + 32)
        .sum::<usize>() as f64
        / (1024.0 * 1024.0);
    match outcome {
        Ok(d) => println!(
            "limits: dim={dim} rows={rows} log={log}: ok in {:.2}s db={:.1} MiB payload~{payload_estimate_mib:.0} MiB rss_peak={:.0} MiB (before insert {before:.0})",
            d.as_secs_f64(),
            file_mib(&path),
            peak_rss_mib()
        ),
        Err(e) => println!(
            "limits: dim={dim} rows={rows} log={log}: REJECTED {e} payload~{payload_estimate_mib:.0} MiB rss_peak={:.0} MiB",
            peak_rss_mib()
        ),
    }
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    match args.first().map(String::as_str) {
        Some("single") => single(&args),
        Some("bulk") => bulk(&args),
        Some("tables") => tables(&args),
        Some("limits") => limits(&args),
        _ => eprintln!("usage: change_log_bench <single|bulk|tables|limits> [key=value ...]"),
    }
}
