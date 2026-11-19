//! 大量 `COPY FROM`（行の形）で ③（バッチ合計バイト。既定 64 MiB）と ④（チャンク数。既定
//! 4096）のどちらが先に効くかと、③ 直下の COPY の所要時間・ピークメモリを測る手動ベンチ
//! （Issue #3310。ポインタ: WIRE-17・INDEX-4）。
//!
//! 行の形から ① を外した（Issue #3308・#3309）あと、DoS 対策の実効上限は ②③④ になった。
//! その中で ③ の既定値が大量 COPY に対して妥当かを実測で確かめるための情報提供専用ベンチで、
//! 既定値は変えない（変更は別判断）。時間依存のため CI・`make ci` には含めない。
//!
//! 表は `docs(embedding VECTOR(d), lang TEXT)`（d は 128・768・1536）。text 形式で各要素は
//! 小数 6 桁。`BatchLimits` は環境変数を読む `default()` を使わず明示する。
//!
//! 構成 A（④ 既定 4096）と構成 B（④ を 1,000,000 へ引き上げ）の 2 通りで、1 MiB 前後の
//! CopyData 相当の塊を `feed` し続け、拒否された時点の行数と原因（`54000`）を記録する。
//! さらに ③ 直下の総量で `feed` → `finish` → `commit_copy_in` の経過時間と、Linux の
//! `/proc/self/status` の `VmHWM`（他 OS では `unavailable`）を出す。ピークメモリはプロセス
//! 単位の累積値のため、d ごとに子プロセスを起動して分離する。
//!
//! fail-closed: COPY の成否・行数の照合が通るまで数値行を出さず、不一致は非 0 終了する。
//! 出力は標準出力のプレーンテキストで、テナント ID・行の値は含めない。
//!
//! 使い方は `make bench-copy-volume`。

use std::process::Command;
use std::time::Instant;

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

const DIM_ENV: &str = "BENCH_COPY_VOLUME_DIM";
const DIMS: [usize; 3] = [128, 768, 1536];
const PIECE_BYTES: usize = 1 << 20;
const TOTAL_LIMIT: usize = engine::incremental::MAX_INDEX_TOTAL_BYTES;
const ROW_LIMIT: usize = engine::chunking::MAX_INPUT_BYTES;
const DEFAULT_CHUNKS: usize = engine::incremental::MAX_CHUNKS_PER_FILE;

fn limits(max_chunks: usize) -> BatchLimits {
    BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: ROW_LIMIT,
        max_batch_total_bytes: TOTAL_LIMIT,
        max_batch_chunks: max_chunks,
    }
}

fn row_text(id: usize, dim: usize) -> String {
    let mut v = String::with_capacity(dim * 9 + 8);
    v.push('[');
    for i in 0..dim {
        if i > 0 {
            v.push(',');
        }
        // 小数 6 桁（実運用の text 形式に近い桁数）。値は決定的。
        let x = ((id * 31 + i * 7) % 1000) as f32 / 1000.0;
        v.push_str(&format!("{x:.6}"));
    }
    v.push(']');
    format!("{id}\t{v}\tja\n")
}

fn open_core(dim: usize, max_chunks: usize) -> (EngineCore, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "fandhe-copy-volume-{}-{dim}-{max_chunks}.redb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(dim as u32), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_batch_limits(limits(max_chunks));
    (core, path)
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("bench", [Visibility::Public, Visibility::Private])
        .expect("valid ctx")
}

fn begin(core: &EngineCore, op: &str) -> engine::sql::copy::CopyInSession {
    let sql = format!("COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID '{op}'");
    match core
        .begin_copy(&ctx(), &SessionState::default(), &sql)
        .expect("begin_copy")
    {
        CopyPlan::From(s) => s,
        CopyPlan::To(..) => panic!("expected FROM plan"),
    }
}

/// 1 MiB 前後の塊（整数行）を作る。`next_id` から始め、塊の行数を返す。
fn piece(next_id: usize, dim: usize, row_bytes: usize) -> (Vec<u8>, usize) {
    let rows = (PIECE_BYTES / row_bytes).max(1);
    let mut out = Vec::with_capacity(rows * row_bytes + 64);
    for i in 0..rows {
        out.extend_from_slice(row_text(next_id + i, dim).as_bytes());
    }
    (out, rows)
}

fn vm_hwm_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find(|l| l.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// 拒否されるまで `feed` し、受理した行数・バイト数と拒否コードを返す（拒否されなければ `None`）。
fn feed_until_rejected(
    core: &EngineCore,
    op: &str,
    dim: usize,
    row_bytes: usize,
    max_pieces: usize,
) -> (usize, usize, Option<String>) {
    let mut s = begin(core, op);
    let mut rows = 0usize;
    let mut bytes = 0usize;
    for _ in 0..max_pieces {
        let (buf, n) = piece(rows + 1, dim, row_bytes);
        match s.feed(&buf) {
            Ok(_) => {
                rows += n;
                bytes += buf.len();
            }
            Err(e) => return (rows, bytes, Some(e.wire_code().to_string())),
        }
    }
    (rows, bytes, None)
}

fn run_one(dim: usize) {
    let row_bytes = row_text(1, dim).len();
    println!(
        "dim={dim} row_bytes={row_bytes} max_rows_within_total={}",
        TOTAL_LIMIT / row_bytes
    );

    // 構成 A: ④ 既定。どちらの上限が先に効くか。
    let (core, path) = open_core(dim, DEFAULT_CHUNKS);
    let (rows, bytes, code) = feed_until_rejected(&core, "cv-a", dim, row_bytes, 200);
    println!(
        "config=A chunk_limit={DEFAULT_CHUNKS} accepted_rows={rows} accepted_bytes={bytes} rejected={}",
        code.as_deref().unwrap_or("none")
    );
    let _ = std::fs::remove_file(&path);

    // 構成 B: ④ を引き上げ、③ に当たるまで。
    let (core, path) = open_core(dim, 1_000_000);
    let (rows_b, bytes_b, code_b) = feed_until_rejected(&core, "cv-b", dim, row_bytes, 200);
    println!(
        "config=B chunk_limit=1000000 accepted_rows={rows_b} accepted_bytes={bytes_b} rejected={}",
        code_b.as_deref().unwrap_or("none")
    );
    let _ = std::fs::remove_file(&path);
    if code_b.as_deref() != Some("54000") {
        eprintln!("copy-volume: expected 54000 once the total limit is exceeded");
        std::process::exit(1);
    }

    // ③ 直下の COPY: feed → finish → commit。
    let (core, path) = open_core(dim, 1_000_000);
    let mut s = begin(&core, "cv-c");
    let started = Instant::now();
    let mut rows = 0usize;
    let mut bytes = 0usize;
    loop {
        let (buf, n) = piece(rows + 1, dim, row_bytes);
        if bytes + buf.len() > TOTAL_LIMIT {
            break;
        }
        s.feed(&buf).expect("feed under the total limit");
        rows += n;
        bytes += buf.len();
    }
    let batch = s.finish().expect("finish");
    let outcome = core.commit_copy_in(&ctx(), batch).expect("commit");
    let elapsed = started.elapsed();
    let _ = std::fs::remove_file(&path);
    if outcome.rows_affected != rows as u64 {
        eprintln!(
            "copy-volume: rows mismatch (fed {rows}, committed {})",
            outcome.rows_affected
        );
        std::process::exit(1);
    }
    let (hwm_s, ratio_s) = match vm_hwm_kib() {
        Some(k) => (
            format!("{} MiB", k / 1024),
            format!("{:.2}", (k * 1024) as f64 / bytes as f64),
        ),
        None => ("unavailable".to_string(), "unavailable".to_string()),
    };
    println!(
        "config=C under_total rows={rows} raw_bytes={bytes} elapsed_ms={} peak_rss={hwm_s} peak_to_raw_ratio={ratio_s}",
        elapsed.as_millis()
    );
}

fn main() {
    if let Ok(v) = std::env::var(DIM_ENV) {
        match v.parse::<usize>() {
            Ok(d) if DIMS.contains(&d) => run_one(d),
            _ => {
                eprintln!("copy-volume: {DIM_ENV} must be one of {DIMS:?}");
                std::process::exit(2);
            }
        }
        return;
    }
    let exe = std::env::current_exe().expect("current_exe");
    for d in DIMS {
        let status = Command::new(&exe)
            .env(DIM_ENV, d.to_string())
            .status()
            .expect("spawn child");
        if !status.success() {
            eprintln!("copy-volume: child for dim={d} failed");
            std::process::exit(1);
        }
    }
}
