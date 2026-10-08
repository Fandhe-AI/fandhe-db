//! チップ別手動計測オーケストレータ（Issue #469・親 #456。ポインタ:
//! `docs/design/ann-index-adoption.md` 系 CORE-9／CORE-10／CORE-16／TASK-132）。
//!
//! 本開発環境（QEMU 仮想 CPU・AVX-512 なし・NEON なし・非現実的なキャッシュ
//! 階層。`docs/design/chip-kernel-guidelines.md` §0.6）では Phase 4（チップ最適
//! カーネル）の採否判定に必要な実測ができないため、オーナー実機
//! （Apple M／AMD Zen 4・5／Intel）での手動計測（`make bench-tier` と同じ運用。
//! Issue #313）を 1 コマンド（`make bench-chip`）で回せるようにする。
//!
//! `dot_kernel_bench`・`knn_profile_bench`・`feature_bench`（dim=128／768）を
//! 1 ワークロード = 1 子プロセスとしてラウンドロビン交互計測し、CPU 情報・
//! 実行時検出 ISA・per-run 生データ・min/median/参照区間帯を `summary.json`
//! （`BENCH_CHIP_OUT_DIR`。既定 `target/bench-chip/<unix-ts>`）へ出力する。
//! 判定ロジック（env パース・行パーサ・集計・JSON 生成）は
//! `harness/chip.rs` に切り出し、`tests/chip_bench_accept.rs`（`make ci` 対象）
//! で時間非依存に検証する。
//!
//! # CI に配線しない・`GITHUB_ACTIONS` 下は拒否
//!
//! `.github/workflows/*` には配線しない（`make bench-chip` からの手動実行専用。
//! `dot_kernel_bench.rs` と同一方針）。
//!
//! production コード（`crates/engine/src/`）は無変更（本 Issue の受け入れ条件）。

#[allow(dead_code)]
mod harness;

use harness::chip::{
    aggregate, dedicated_env_attested, json_escape, json_number, json_number_opt, parse_cache_size,
    parse_dot_kernel_diag_line, parse_dot_kernel_line, parse_feature_bench_output,
    parse_knn_stage_line, parse_proc_cpuinfo, parse_rounds, parse_sysctl_lines, parse_workloads,
    refuse_under_github_actions, ChipError, DotKernelDiag, MetricSeries, Workload,
    AARCH64_INTEREST_FLAGS, POLICY_MIN_ROUNDS, X86_INTEREST_FLAGS,
};
use harness::env_report::EnvReport;

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// `GITHUB_ACTIONS` の設定有無（`dot_kernel_bench.rs::running_under_github_actions`
/// と同一パターン）。
fn running_under_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

/// 子プロセスの stdout／stderr 読み取り上限（無制限確保防止。1 ラウンド分の
/// 出力は数十 KiB 程度のため十分な余裕を持たせる）。
const CHILD_OUTPUT_MAX_BYTES: usize = 16 * 1024 * 1024;

/// ワークスペースルート（`CARGO_MANIFEST_DIR` が `crates/engine`）を解決する。
fn workspace_root() -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    std::path::Path::new(&manifest_dir)
        .join("../..")
        .to_path_buf()
}

/// `cargo` 実体のパス（`cargo bench` から起動されたプロセスは `CARGO` env を
/// 設定する契約。未設定時は `"cargo"` を PATH から解決する）。
fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// `Command` の出力（stdout／stderr）を上限付きで読み切る（無制限 `Vec` 確保
/// 防止。上限超過は fail-closed に `Err`）。
fn read_capped(mut reader: impl Read) -> Result<String, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            break;
        }
        if buf.len() + n > CHILD_OUTPUT_MAX_BYTES {
            return Err(format!(
                "child output exceeds {CHILD_OUTPUT_MAX_BYTES} bytes"
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 1 (round, workload) 分の子プロセス実行結果。
struct RunOutcome {
    exit_code: i32,
    stdout: String,
    #[allow(dead_code)]
    stderr: String,
}

/// `workload` を 1 子プロセスとして起動し完了まで待つ（固定 argv・シェル不使用。
/// env は `extra_env` に加え親環境をそのまま継承する契約——README 参照）。
fn run_workload(workload: Workload) -> Result<RunOutcome, String> {
    let cargo = cargo_bin();
    let root = workspace_root();
    let mut cmd = Command::new(&cargo);
    cmd.args(workload.cargo_args(env!("CARGO_PKG_NAME")));
    cmd.current_dir(&root);
    for (k, v) in workload.extra_env() {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {e}", workload.token()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing child stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "missing child stderr".to_string())?;
    // stdout・stderr を別スレッドで並行に読み切る（子側のパイプバッファ枯渇に
    // よるデッドロック防止。片方だけを同期的に読み切ると、もう片方のパイプが
    // 満杯になった時点で子プロセスが書き込みブロックし、親の `wait()` も
    // 進まなくなる）。
    let stdout_handle = std::thread::spawn(move || read_capped(stdout));
    let stderr_handle = std::thread::spawn(move || read_capped(stderr));
    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for {}: {e}", workload.token()))?;
    let stdout = stdout_handle
        .join()
        .map_err(|_| "stdout reader thread panicked".to_string())??;
    let stderr = stderr_handle
        .join()
        .map_err(|_| "stderr reader thread panicked".to_string())??;
    Ok(RunOutcome {
        exit_code: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

/// `/proc/loadavg`（Linux）または `sysctl -n vm.loadavg`（macOS）を best-effort
/// で読む。読めない環境では `"unavailable"`。
fn read_loadavg() -> String {
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") {
        if let Some(line) = s.lines().next() {
            return line.to_string();
        }
    }
    if std::env::consts::OS == "macos" {
        if let Ok(out) = Command::new("sysctl").args(["-n", "vm.loadavg"]).output() {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
        }
    }
    "unavailable".to_string()
}

/// CPU 情報を best-effort で集める。Linux は `/proc/cpuinfo`、macOS は
/// `sysctl` 固定キーリストを読む。取得失敗はフィールドを空のまま残す
/// （収集失敗が計測本体を止める理由にはならない。`env_report.rs` と同方針）。
fn collect_cpu_info() -> (Option<String>, Vec<String>, Vec<(String, String)>) {
    if std::env::consts::OS == "macos" {
        let keys = [
            "machdep.cpu.brand_string",
            "hw.ncpu",
            "hw.perflevel0.physicalcpu",
            "hw.perflevel1.physicalcpu",
            "hw.l2cachesize",
            "hw.cachelinesize",
            "hw.optional.arm.FEAT_FP16",
            "hw.optional.arm.FEAT_DotProd",
            "hw.optional.arm.FEAT_BF16",
            "hw.optional.arm.FEAT_I8MM",
            "hw.optional.arm.FEAT_SME",
            "hw.optional.avx2_0",
            "hw.optional.avx512f",
        ];
        let mut args = vec![];
        args.extend_from_slice(&keys);
        let sysctl = Command::new("sysctl").args(&args).output();
        let mut model_name = None;
        let mut sysctl_pairs = Vec::new();
        if let Ok(out) = sysctl {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            sysctl_pairs = parse_sysctl_lines(&text);
            for (k, v) in &sysctl_pairs {
                if k == "machdep.cpu.brand_string" {
                    model_name = Some(v.clone());
                }
            }
        }
        (model_name, Vec::new(), sysctl_pairs)
    } else {
        let interest: &[&str] = if std::env::consts::ARCH == "aarch64" {
            AARCH64_INTEREST_FLAGS
        } else {
            X86_INTEREST_FLAGS
        };
        let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let info = parse_proc_cpuinfo(&text, interest);
        (info.model_name, info.flags, Vec::new())
    }
}

/// Linux のキャッシュ容量を `/sys/devices/system/cpu/cpu0/cache/index*/` から
/// best-effort で読む（macOS は `hw.l2cachesize` 等を `collect_cpu_info` 側で
/// 別途扱うため対象外）。
fn collect_linux_caches() -> Vec<(u32, String, u64)> {
    let mut out = Vec::new();
    for idx in 0..8u32 {
        let base = format!("/sys/devices/system/cpu/cpu0/cache/index{idx}");
        let level = std::fs::read_to_string(format!("{base}/level"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        let ty = std::fs::read_to_string(format!("{base}/type"))
            .ok()
            .map(|s| s.trim().to_string());
        let size_raw = std::fs::read_to_string(format!("{base}/size")).ok();
        let (Some(level), Some(ty), Some(size_raw)) = (level, ty, size_raw) else {
            continue;
        };
        if let Some(bytes) = parse_cache_size(&size_raw) {
            out.push((level, ty, bytes));
        }
    }
    out
}

/// x86_64 の実行時検出フラグ（`is_x86_feature_detected!` は安全なマクロ。
/// unsafe を要さない）。
#[cfg(target_arch = "x86_64")]
fn runtime_features() -> Vec<(&'static str, bool)> {
    vec![
        ("avx2", is_x86_feature_detected!("avx2")),
        ("fma", is_x86_feature_detected!("fma")),
        ("f16c", is_x86_feature_detected!("f16c")),
        ("avx512f", is_x86_feature_detected!("avx512f")),
        ("avx512bw", is_x86_feature_detected!("avx512bw")),
        ("avx512vl", is_x86_feature_detected!("avx512vl")),
        ("avx512vnni", is_x86_feature_detected!("avx512vnni")),
        ("avx512bf16", is_x86_feature_detected!("avx512bf16")),
        ("avx512fp16", is_x86_feature_detected!("avx512fp16")),
        ("avxvnni", is_x86_feature_detected!("avxvnni")),
    ]
}

/// aarch64 の実行時検出フラグ（Issue #468 の材料。結論はここでは書かない）。
#[cfg(target_arch = "aarch64")]
fn runtime_features() -> Vec<(&'static str, bool)> {
    vec![
        ("neon", std::arch::is_aarch64_feature_detected!("neon")),
        ("fp16", std::arch::is_aarch64_feature_detected!("fp16")),
        ("fhm", std::arch::is_aarch64_feature_detected!("fhm")),
        (
            "dotprod",
            std::arch::is_aarch64_feature_detected!("dotprod"),
        ),
        ("bf16", std::arch::is_aarch64_feature_detected!("bf16")),
        ("i8mm", std::arch::is_aarch64_feature_detected!("i8mm")),
        ("sve", std::arch::is_aarch64_feature_detected!("sve")),
        ("sve2", std::arch::is_aarch64_feature_detected!("sve2")),
        // "sme" は stable Rust では stdarch_aarch64_feature_detection
        // （https://github.com/rust-lang/rust/issues/127764）が未安定のため
        // is_aarch64_feature_detected! で検出できない（cross-check の aarch64
        // クロスコンパイルで E0658）。安定化まで計測項目から除外する。
    ]
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn runtime_features() -> Vec<(&'static str, bool)> {
    Vec::new()
}

/// ビルド情報（`git`・`rustc`。best-effort。`rev-parse`／`status --porcelain`
/// のみで remote へは触れない）。
fn collect_build_info() -> (String, bool, String) {
    let root = workspace_root();
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&root)
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let rustc_version = Command::new(&rustc)
        .arg("-vV")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    (commit, dirty, rustc_version)
}

/// 1 (round, workload) 分の実測メトリクス（キー→値 1 点）。集計は
/// `chip_bench.rs::main` がラウンドをまたいでキーごとに束ねる。
type MetricPoint = BTreeMap<String, f64>;

/// `dot_kernel_bench` の stdout からメトリクス点を作る。
fn metrics_from_dot_kernel(stdout: &str) -> Result<MetricPoint, ChipError> {
    let mut points = MetricPoint::new();
    let mut found_any = false;
    for line in stdout.lines() {
        if let Some(sample) = parse_dot_kernel_line(line) {
            found_any = true;
            points.insert(
                format!("{}/dim={}/ns_per_dot", sample.working_set, sample.dim),
                sample.ns_per_dot,
            );
            points.insert(
                format!("{}/dim={}/median_ms", sample.working_set, sample.dim),
                sample.median_ms,
            );
        } else if let Some(diag) = parse_dot_kernel_diag_line(line) {
            let DotKernelDiag {
                simd_vs_scalar_ratio,
                ..
            } = diag;
            points.insert(
                "diagnostic_ab/simd_vs_scalar_ratio".to_string(),
                simd_vs_scalar_ratio,
            );
        }
    }
    if !found_any {
        return Err(ChipError::Empty(
            "no dot_kernel sample lines found in stdout".to_string(),
        ));
    }
    Ok(points)
}

/// `knn_profile_bench` の stdout からメトリクス点を作る。
fn metrics_from_knn_profile(stdout: &str) -> Result<MetricPoint, ChipError> {
    let mut points = MetricPoint::new();
    for line in stdout.lines() {
        if let Some(sample) = parse_knn_stage_line(line) {
            points.insert(format!("{}/median_ms", sample.name), sample.median_ms);
            points.insert(format!("{}/ns_per_row", sample.name), sample.ns_per_row);
        }
    }
    if points.is_empty() {
        return Err(ChipError::Empty(
            "no knn_profile stage lines found in stdout".to_string(),
        ));
    }
    Ok(points)
}

/// `feature_bench` の stdout からメトリクス点を作る。
fn metrics_from_feature_bench(
    stdout: &str,
    expected_dim: u32,
) -> Result<(MetricPoint, String, u64, u64), ChipError> {
    let result = parse_feature_bench_output(stdout, expected_dim)?;
    let mut points = MetricPoint::new();
    for phase in &result.phases {
        points.insert(format!("{}/min_us", phase.name), phase.min_us);
        points.insert(format!("{}/p50_us", phase.name), phase.p50_us);
        points.insert(format!("{}/p95_us", phase.name), phase.p95_us);
    }
    Ok((points, result.engine, result.scale, result.rows_total))
}

/// 与えられたワークロードの `expected_dim`（feature_128／feature_768 のみ意味を
/// 持つ。他は 0 を返し呼び出し元は無視する）。
fn expected_dim_for(workload: Workload) -> u32 {
    match workload {
        Workload::Feature128 => 128,
        Workload::Feature768 => 768,
        Workload::DotKernel | Workload::KnnProfile => 0,
    }
}

fn fail_closed(msg: impl std::fmt::Display) -> ! {
    eprintln!("chip_bench: {msg}");
    std::process::exit(1);
}

fn main() {
    if let Err(e) = refuse_under_github_actions(running_under_github_actions()) {
        fail_closed(e);
    }

    let rounds = match std::env::var("BENCH_CHIP_ROUNDS") {
        Ok(v) => match parse_rounds(Some(&v)) {
            Ok(r) => r,
            Err(e) => fail_closed(e),
        },
        Err(_) => match parse_rounds(None) {
            Ok(r) => r,
            Err(e) => fail_closed(e),
        },
    };
    let workloads_raw = std::env::var("BENCH_CHIP_WORKLOADS").ok();
    let workloads = match parse_workloads(workloads_raw.as_deref()) {
        Ok(w) => w,
        Err(e) => fail_closed(e),
    };
    let dedicated_raw = std::env::var("BENCH_DEDICATED_ENV").ok();
    let dedicated = dedicated_env_attested(dedicated_raw.as_deref());

    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out_dir_raw = std::env::var("BENCH_CHIP_OUT_DIR")
        .unwrap_or_else(|_| format!("target/bench-chip/{unix_secs}"));
    let out_dir = workspace_root().join(&out_dir_raw);
    if out_dir.join("summary.json").exists() {
        fail_closed(format!(
            "refusing to overwrite existing summary.json under {}",
            out_dir.display()
        ));
    }
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        fail_closed(format!("failed to create {}: {e}", out_dir.display()));
    }

    let detected_isa = format!("{:?}", engine::isa::current().isa());
    let env = EnvReport::capture(detected_isa.clone());
    println!("{env}");
    if !dedicated {
        println!(
            "chip_bench: BENCH_DEDICATED_ENV not set; treat results as shared-environment \
             reference values only, not as an adoption basis (benchmark-judgement-policy.md §5)"
        );
    }

    let (model_name, cpuinfo_flags, sysctl_pairs) = collect_cpu_info();
    let caches = collect_linux_caches();
    let features = runtime_features();
    let (commit, dirty, rustc) = collect_build_info();

    // ラウンド × ワークロードの実測点を集める（1 ワークロード = 1 子プロセス。
    // 複数規模点の同一プロセス内逐次測定は比較不能な証拠と判明済み
    // 〔benchmark-judgement-policy.md §5〕のため、ここでも 1 プロセス = 1 規模点
    // を feature_128／feature_768 それぞれの独立子プロセスで担保する）。
    let mut per_workload_points: BTreeMap<Workload, Vec<MetricPoint>> = BTreeMap::new();
    let mut feature_meta: BTreeMap<Workload, (String, u64, u64)> = BTreeMap::new();
    let mut run_log = Vec::new();

    for round in 1..=rounds {
        for &workload in &workloads {
            let loadavg_before = read_loadavg();
            let outcome = match run_workload(workload) {
                Ok(o) => o,
                Err(e) => fail_closed(format!("round={round} workload={}: {e}", workload.token())),
            };
            let stdout_log = out_dir.join(format!("round{round}_{}.stdout.log", workload.token()));
            let stderr_log = out_dir.join(format!("round{round}_{}.stderr.log", workload.token()));
            // per-run ログの保存失敗を握りつぶすと summary.json だけは正常生成され、
            // 存在しないログを runs[].stdout_log が指したまま正常終了してしまう
            // （codex-review 指摘）。書き込み結果を確認し失敗時は fail-closed で
            // 異常終了する。
            if let Err(e) = std::fs::write(&stdout_log, &outcome.stdout) {
                fail_closed(format!("failed to write {}: {e}", stdout_log.display()));
            }
            if let Err(e) = std::fs::write(&stderr_log, &outcome.stderr) {
                fail_closed(format!("failed to write {}: {e}", stderr_log.display()));
            }
            println!(
                "chip_bench: round={round}/{rounds} workload={} exit={}",
                workload.token(),
                outcome.exit_code
            );
            if outcome.exit_code != 0 {
                fail_closed(format!(
                    "round={round} workload={} exited with code {} (see {})",
                    workload.token(),
                    outcome.exit_code,
                    stderr_log.display()
                ));
            }
            let parsed = match workload {
                Workload::DotKernel => metrics_from_dot_kernel(&outcome.stdout),
                Workload::KnnProfile => metrics_from_knn_profile(&outcome.stdout),
                Workload::Feature128 | Workload::Feature768 => {
                    match metrics_from_feature_bench(&outcome.stdout, expected_dim_for(workload)) {
                        Ok((points, engine_name, scale, rows_total)) => {
                            feature_meta.insert(workload, (engine_name, scale, rows_total));
                            Ok(points)
                        }
                        Err(e) => Err(e),
                    }
                }
            };
            let points = match parsed {
                Ok(p) => p,
                Err(e) => fail_closed(format!(
                    "round={round} workload={}: {e} (see {})",
                    workload.token(),
                    stdout_log.display()
                )),
            };
            per_workload_points
                .entry(workload)
                .or_default()
                .push(points);
            run_log.push((
                round,
                workload,
                loadavg_before,
                outcome.exit_code,
                stdout_log
                    .strip_prefix(&out_dir)
                    .unwrap_or(&stdout_log)
                    .display()
                    .to_string(),
            ));
        }
    }

    // ラウンドをまたいだキー集合の一致検証 + 集計。
    let mut results: BTreeMap<Workload, BTreeMap<String, MetricSeries>> = BTreeMap::new();
    for (workload, points_per_round) in &per_workload_points {
        let mut keys: Option<Vec<String>> = None;
        for points in points_per_round {
            let mut ks: Vec<String> = points.keys().cloned().collect();
            ks.sort();
            match &keys {
                None => keys = Some(ks),
                Some(existing) => {
                    if existing != &ks {
                        fail_closed(format!(
                            "workload={}: metric key set differs across rounds",
                            workload.token()
                        ));
                    }
                }
            }
        }
        let Some(keys) = keys else { continue };
        let mut per_metric = BTreeMap::new();
        for key in &keys {
            let values: Vec<f64> = points_per_round
                .iter()
                .filter_map(|p| p.get(key).copied())
                .collect();
            match aggregate(&values) {
                Ok(series) => {
                    per_metric.insert(key.clone(), series);
                }
                Err(e) => fail_closed(format!("workload={} metric={key}: {e}", workload.token())),
            }
        }
        results.insert(*workload, per_metric);
    }

    // ---------------------------------------------------------------
    // summary.json 出力
    // ---------------------------------------------------------------
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"schema_version\": 1,\n");
    out.push_str("  \"label\": \"bench_chip\",\n");
    out.push_str(&format!("  \"generated_unix_secs\": {unix_secs},\n"));
    out.push_str(&format!("  \"rounds\": {rounds},\n"));
    out.push_str(&format!("  \"policy_min_rounds\": {POLICY_MIN_ROUNDS},\n"));
    out.push_str(&format!(
        "  \"meets_policy_min_rounds\": {},\n",
        rounds >= POLICY_MIN_ROUNDS
    ));
    out.push_str(&format!("  \"dedicated_env_attested\": {dedicated},\n"));
    out.push_str("  \"workloads\": [");
    for (i, w) in workloads.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}\"", w.token()));
    }
    out.push_str("],\n");
    out.push_str("  \"build\": {");
    out.push_str(&format!(
        "\"commit\": \"{}\", \"dirty\": {dirty}, \"rustc\": \"{}\", \"host\": \"{}\"",
        json_escape(&commit),
        json_escape(&rustc),
        json_escape(std::env::consts::ARCH)
    ));
    out.push_str("},\n");

    out.push_str("  \"env\": {\n");
    out.push_str(&format!(
        "    \"os\": \"{}\", \"arch\": \"{}\", \"logical_cpus\": {}, \"detected_isa\": \"{}\",\n",
        json_escape(env.os),
        json_escape(env.arch),
        env.logical_cpus,
        json_escape(&detected_isa)
    ));
    out.push_str("    \"cpu\": {");
    out.push_str(&format!(
        "\"model_name\": {}, \"flags\": [",
        model_name
            .as_ref()
            .map(|m| format!("\"{}\"", json_escape(m)))
            .unwrap_or_else(|| "null".to_string())
    ));
    for (i, f) in cpuinfo_flags.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}\"", json_escape(f)));
    }
    out.push_str("], \"caches\": [");
    for (i, (level, ty, bytes)) in caches.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"level\": {level}, \"type\": \"{}\", \"bytes\": {bytes}}}",
            json_escape(ty)
        ));
    }
    out.push_str("], \"sysctl\": {");
    for (i, (k, v)) in sysctl_pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}\": \"{}\"", json_escape(k), json_escape(v)));
    }
    out.push_str("}},\n");
    out.push_str("    \"runtime_features\": {");
    for (i, (name, value)) in features.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{name}\": {value}"));
    }
    out.push_str("}\n");
    out.push_str("  },\n");

    out.push_str("  \"runs\": [\n");
    for (i, (round, workload, loadavg, exit_code, log_path)) in run_log.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!(
            "    {{\"round\": {round}, \"workload\": \"{}\", \"loadavg_before\": \"{}\", \
             \"exit_code\": {exit_code}, \"stdout_log\": \"{}\"}}",
            workload.token(),
            json_escape(loadavg),
            json_escape(log_path)
        ));
    }
    out.push_str("\n  ],\n");

    out.push_str("  \"results\": {\n");
    for (i, (workload, metrics)) in results.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!("    \"{}\": {{", workload.token()));
        if let Some((engine_name, scale, rows_total)) = feature_meta.get(workload) {
            out.push_str(&format!(
                "\"engine\": \"{}\", \"scale\": {scale}, \"rows_total\": {rows_total}, ",
                json_escape(engine_name)
            ));
        }
        out.push_str("\"metrics\": {");
        for (j, (key, series)) in metrics.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\"{}\": {{\"values\": [{}], \"min\": {}, \"median\": {}, \"max\": {}, \
                 \"reference_band_pct\": {}}}",
                json_escape(key),
                series
                    .values
                    .iter()
                    .map(|v| json_number(*v))
                    .collect::<Vec<_>>()
                    .join(","),
                json_number(series.min),
                json_number(series.median),
                json_number(series.max),
                json_number_opt(series.reference_band_pct)
            ));
        }
        out.push_str("}}");
    }
    out.push_str("\n  },\n");
    // raw_logs_dir は summary.json 自身の位置からの相対パスのみを記録する
    // （`runs[].stdout_log` と同じ基準）。per-run ログは out_dir 直下
    // （summary.json と同じディレクトリ）に書くため常に "." であり、
    // BENCH_CHIP_OUT_DIR に絶対パスを渡された場合でもユーザー名を含む
    // ローカルパスが summary.json へ残らない
    // （codex-review 指摘・chip-kernel-guidelines.md §7.4）。
    out.push_str("  \"raw_logs_dir\": \".\"\n");
    out.push_str("}\n");

    let summary_path = out_dir.join("summary.json");
    if let Err(e) = std::fs::write(&summary_path, &out) {
        fail_closed(format!("failed to write {}: {e}", summary_path.display()));
    }
    println!("{out}");
}
