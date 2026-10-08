//! `is_aarch64_feature_detected!` / `is_x86_feature_detected!` の実効性を実機で
//! 確認するための手動実行専用ツール（Issue #468・TASK-156／CORE-14 ポインタ）。
//!
//! `std::arch::is_aarch64_feature_detected!` のマクロ doc には「linux 系以外の
//! OS では多くの feature の実行時検出が常に `false` を返す」という注記があり、
//! これが真であれば Apple Silicon 上で `crates/engine/src/isa.rs::NeonToken`
//! が `None` を返し既存 NEON カーネル・Phase 4（#459 配下）の Apple 向けカーネル
//! が fail-closed 側で無効化される懸念があった。本ツールはその懸念を実機で検証
//! するための素材（コンパイル時 `cfg!(target_feature)` 列・マクロ結果列・
//! macOS では `sysctl` による相互検証列）を表として出力する。
//!
//! `cargo run -p fandhe-db-engine --release --example detect_features`（`make
//! detect-features`）で実行し、出力を
//! `docs/design/chip-kernel-guidelines.md` の該当節へ転記する運用とする。
//!
//! 依存追加は行わない（std のみ）。検出結果を上書きする環境変数・引数は一切
//! 設けない（`isa.rs` モジュールドキュメントの CORE-12 節・
//! `tests/isa.rs::isa_source_has_no_external_override_entry_points` と同じ
//! fail-closed の姿勢を本ツールでも維持する）。

/// 1 行分の検出結果（feature 名・コンパイル時定数・マクロ実行結果・sysctl 相互検証）。
///
/// `detected` は `&'static str` にしている。`sme`／`sme2` は本リポの
/// `rust-toolchain.toml`（stable）では `is_aarch64_feature_detected!` マクロが
/// `stdarch_aarch64_feature_detection` 機能ゲート未安定のためコンパイルできず
/// （`rustc --explain E0658`）、この 2 feature のみ `"n/a (unstable macro)"` の
/// 固定文字列を入れ、他 feature は `bool` の文字列化（`true`/`false`）を入れる。
struct Row {
    feature: &'static str,
    compile_time: bool,
    detected: &'static str,
    sysctl: &'static str,
}

fn print_table(rows: &[Row]) {
    println!("| feature | cfg!(target_feature) | is_*_feature_detected! | sysctl |");
    println!("| --- | --- | --- | --- |");
    for row in rows {
        println!(
            "| {} | {} | {} | {} |",
            row.feature, row.compile_time, row.detected, row.sysctl
        );
    }
}

/// macOS 上で `sysctl -n <name>` を実行し、`std::arch::is_aarch64_feature_detected!`
/// の結果と独立に対応可否を確認する相互検証列を作る。
///
/// untrusted 入力は扱わない（`name` は本ファイル内の固定文字列のみを渡す）。
/// コマンド不在・キー不在・パース不能などいかなる失敗も検出結果へは影響させず
/// `"n/a"` として表示する（fail-closed。検出結果の上書き経路にはしない）。
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn sysctl_bool(name: &str) -> &'static str {
    let output = match std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
    {
        Ok(output) => output,
        Err(_) => return "n/a",
    };
    if !output.status.success() {
        return "n/a";
    }
    let Ok(stdout) = String::from_utf8(output.stdout) else {
        return "n/a";
    };
    match stdout.trim() {
        "1" => "true",
        "0" => "false",
        _ => "n/a",
    }
}

#[cfg(all(target_arch = "aarch64", not(target_os = "macos")))]
fn sysctl_bool(_name: &str) -> &'static str {
    "n/a"
}

#[cfg(target_arch = "aarch64")]
fn feature_rows() -> Vec<Row> {
    vec![
        Row {
            feature: "neon",
            compile_time: cfg!(target_feature = "neon"),
            detected: if std::arch::is_aarch64_feature_detected!("neon") {
                "true"
            } else {
                "false"
            },
            sysctl: sysctl_bool("hw.optional.AdvSIMD"),
        },
        Row {
            feature: "fp16",
            compile_time: cfg!(target_feature = "fp16"),
            detected: if std::arch::is_aarch64_feature_detected!("fp16") {
                "true"
            } else {
                "false"
            },
            sysctl: sysctl_bool("hw.optional.arm.FEAT_FP16"),
        },
        Row {
            feature: "fhm",
            compile_time: cfg!(target_feature = "fhm"),
            detected: if std::arch::is_aarch64_feature_detected!("fhm") {
                "true"
            } else {
                "false"
            },
            sysctl: sysctl_bool("hw.optional.arm.FEAT_FHM"),
        },
        Row {
            feature: "dotprod",
            compile_time: cfg!(target_feature = "dotprod"),
            detected: if std::arch::is_aarch64_feature_detected!("dotprod") {
                "true"
            } else {
                "false"
            },
            sysctl: sysctl_bool("hw.optional.arm.FEAT_DotProd"),
        },
        Row {
            feature: "bf16",
            compile_time: cfg!(target_feature = "bf16"),
            detected: if std::arch::is_aarch64_feature_detected!("bf16") {
                "true"
            } else {
                "false"
            },
            sysctl: sysctl_bool("hw.optional.arm.FEAT_BF16"),
        },
        Row {
            feature: "i8mm",
            compile_time: cfg!(target_feature = "i8mm"),
            detected: if std::arch::is_aarch64_feature_detected!("i8mm") {
                "true"
            } else {
                "false"
            },
            sysctl: sysctl_bool("hw.optional.arm.FEAT_I8MM"),
        },
        Row {
            feature: "sme",
            compile_time: cfg!(target_feature = "sme"),
            detected: "n/a (unstable macro)",
            sysctl: sysctl_bool("hw.optional.arm.FEAT_SME"),
        },
        Row {
            feature: "sme2",
            compile_time: cfg!(target_feature = "sme2"),
            detected: "n/a (unstable macro)",
            sysctl: sysctl_bool("hw.optional.arm.FEAT_SME2"),
        },
    ]
}

#[cfg(target_arch = "x86_64")]
fn feature_rows() -> Vec<Row> {
    vec![
        Row {
            feature: "avx2",
            compile_time: cfg!(target_feature = "avx2"),
            detected: if std::arch::is_x86_feature_detected!("avx2") {
                "true"
            } else {
                "false"
            },
            sysctl: "n/a",
        },
        Row {
            feature: "fma",
            compile_time: cfg!(target_feature = "fma"),
            detected: if std::arch::is_x86_feature_detected!("fma") {
                "true"
            } else {
                "false"
            },
            sysctl: "n/a",
        },
        Row {
            feature: "f16c",
            compile_time: cfg!(target_feature = "f16c"),
            detected: if std::arch::is_x86_feature_detected!("f16c") {
                "true"
            } else {
                "false"
            },
            sysctl: "n/a",
        },
        Row {
            feature: "avx512f",
            compile_time: cfg!(target_feature = "avx512f"),
            detected: if std::arch::is_x86_feature_detected!("avx512f") {
                "true"
            } else {
                "false"
            },
            sysctl: "n/a",
        },
        Row {
            feature: "avx512vnni",
            compile_time: cfg!(target_feature = "avx512vnni"),
            detected: if std::arch::is_x86_feature_detected!("avx512vnni") {
                "true"
            } else {
                "false"
            },
            sysctl: "n/a",
        },
        Row {
            feature: "avx512bf16",
            compile_time: cfg!(target_feature = "avx512bf16"),
            detected: if std::arch::is_x86_feature_detected!("avx512bf16") {
                "true"
            } else {
                "false"
            },
            sysctl: "n/a",
        },
        Row {
            feature: "avxvnni",
            compile_time: cfg!(target_feature = "avxvnni"),
            detected: if std::arch::is_x86_feature_detected!("avxvnni") {
                "true"
            } else {
                "false"
            },
            sysctl: "n/a",
        },
    ]
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
fn feature_rows() -> Vec<Row> {
    Vec::new()
}

fn main() {
    println!("arch: {}", std::env::consts::ARCH);
    println!("os: {}", std::env::consts::OS);
    println!("isa::current(): {:?}", engine::isa::current().isa());
    // Issue #525: `detect-apple` の step summary へ `NeonDotprod` が実際に
    // 選ばれたことの直接証跡を残す（`isa::current_i8()` は VNNI 系（x86_64）・
    // NeonDotprod（aarch64）・Scalar の実行時ディスパッチ結果）。
    println!("isa::current_i8(): {:?}", engine::isa::current_i8().isa());
    println!();

    let rows = feature_rows();
    if rows.is_empty() {
        println!("no runtime detection table for this arch");
        return;
    }
    print_table(&rows);
}
