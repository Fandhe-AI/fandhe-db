//! wgpu アダプタの features／limits を実機で確認するための手動実行専用ツール
//! （Issue #535・親 #534・Phase 5 親 #460）。
//!
//! `gpu_batch.rs::init_gpu_context` が `request_adapter` する条件と同じ条件
//! （`InstanceDescriptor::new_without_display_handle`・
//! `PowerPreference::HighPerformance`・`force_fallback_adapter: false`）で
//! adapter のみを取得し、`docs/design/gpu-batch-topk.md` の RTX 3060 実測表
//! （`Features::SUBGROUP`／`SUBGROUP_BARRIER`／`SUBGROUP_VERTEX` の可否・
//! `AdapterInfo::subgroup_min_size`／`subgroup_max_size`・主要 `Limits`）を
//! 再現するための素材を出力する。デバイスは生成しない（シェーダ検証は行わず、
//! 検証結果は同 doc に実測値として記録済み）。
//!
//! `cargo run -p fandhe-db-engine --release --example gpu_adapter_info`（`make
//! gpu-adapter-info`）で実行する。`detect_features.rs`（Issue #468）と同じ
//! 「手動専用・CI 非配線・出力を doc へ転記する運用」の位置づけで、
//! GPU 非搭載環境では adapter 未検出をエラーとして明示し非 0 終了する
//! （silent success にしない）。
//!
//! 依存追加は行わない（wgpu は既存依存 `=30.0.1` のみ）。検出結果を上書きする
//! 環境変数・引数・`from_env` 系は一切使わない（`isa.rs` モジュールドキュメント
//! の CORE-12 節・`gpu_batch.rs::init_gpu_context` と同じ fail-closed の姿勢を
//! 本ツールでも維持する）。`unsafe` は使わない（`Waker::noop()` による
//! 自己ポーリングのみで future を駆動する。`gpu_batch.rs::pollster_free_block_on`
//! と同じ方式をこのツール内に複製する。private 関数のため crate 外から
//! 再利用できないことによる意図的な重複）。

use std::time::Duration;

/// `gpu_batch.rs::GPU_POLL_DEADLINE` と同じ意図（ドライバ無応答時に永久停止
/// しないための打ち切り時間）。値は本ツール専用に独立して持つ
/// （production 定数への依存を作らない）。
const POLL_DEADLINE: Duration = Duration::from_secs(5);

/// std-only の同期化ヘルパー（`pollster` 依存を追加しない方針。
/// `gpu_batch.rs::pollster_free_block_on` と同じ実装をこのツール内に複製）。
fn block_on<F: std::future::Future>(fut: F) -> Result<F::Output, ()> {
    use std::task::{Context, Poll, Waker};

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut boxed = Box::pin(fut);
    let deadline = std::time::Instant::now() + POLL_DEADLINE;
    loop {
        match boxed.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return Ok(v),
            Poll::Pending => {
                if std::time::Instant::now() >= deadline {
                    return Err(());
                }
                std::thread::yield_now();
            }
        }
    }
}

fn main() {
    if wgpu::Instance::enabled_backend_features().is_empty() {
        eprintln!("error: no wgpu backend compiled into this binary");
        std::process::exit(1);
    }

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

    let adapter_result = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
        apply_limit_buckets: false,
    }));

    let adapter = match adapter_result {
        Err(()) => {
            eprintln!("error: adapter request timed out");
            std::process::exit(1);
        }
        Ok(Err(e)) => {
            eprintln!("error: adapter request failed: {e}");
            std::process::exit(1);
        }
        Ok(Ok(adapter)) => adapter,
    };

    let info = adapter.get_info();

    // `gpu_batch.rs::init_gpu_context` と同じ契約: lavapipe 等のソフトウェア
    // adapter は「GPU 搭載環境」の代替にならないため拒否する。これを省くと
    // GPU 非搭載でもソフトウェア adapter が使える環境で本ツールが正常終了
    // してしまい、冒頭コメントが謳う「GPU 非搭載環境では非 0 終了」の契約に
    // 反する（本番経路と診断ツールで adapter 受理条件を一致させる）。
    if info.device_type == wgpu::DeviceType::Cpu {
        eprintln!("error: adapter is a software (CPU) implementation");
        std::process::exit(1);
    }

    let features = adapter.features();
    let limits = adapter.limits();

    println!("## adapter");
    println!("| field | value |");
    println!("| --- | --- |");
    println!("| name | {} |", info.name);
    println!("| backend | {:?} |", info.backend);
    println!("| device_type | {:?} |", info.device_type);
    println!("| driver | {} |", info.driver);
    println!("| driver_info | {} |", info.driver_info);
    println!("| subgroup_min_size | {} |", info.subgroup_min_size);
    println!("| subgroup_max_size | {} |", info.subgroup_max_size);

    println!();
    println!("## features");
    println!("| feature | supported |");
    println!("| --- | --- |");
    println!(
        "| SUBGROUP | {} |",
        features.contains(wgpu::Features::SUBGROUP)
    );
    println!(
        "| SUBGROUP_BARRIER | {} |",
        features.contains(wgpu::Features::SUBGROUP_BARRIER)
    );
    println!(
        "| SUBGROUP_VERTEX | {} |",
        features.contains(wgpu::Features::SUBGROUP_VERTEX)
    );
    println!(
        "| SHADER_F16 | {} |",
        features.contains(wgpu::Features::SHADER_F16)
    );
    println!(
        "| SHADER_INT64 | {} |",
        features.contains(wgpu::Features::SHADER_INT64)
    );
    println!(
        "| TIMESTAMP_QUERY | {} |",
        features.contains(wgpu::Features::TIMESTAMP_QUERY)
    );
    println!(
        "| MAPPABLE_PRIMARY_BUFFERS | {} |",
        features.contains(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS)
    );

    println!();
    println!("## limits（主要項目）");
    println!("| limit | value |");
    println!("| --- | --- |");
    println!(
        "| max_compute_workgroup_storage_size | {} |",
        limits.max_compute_workgroup_storage_size
    );
    println!(
        "| max_compute_invocations_per_workgroup | {} |",
        limits.max_compute_invocations_per_workgroup
    );
    println!(
        "| max_compute_workgroup_size_x | {} |",
        limits.max_compute_workgroup_size_x
    );
    println!(
        "| max_compute_workgroups_per_dimension | {} |",
        limits.max_compute_workgroups_per_dimension
    );
    println!(
        "| max_storage_buffer_binding_size | {} |",
        limits.max_storage_buffer_binding_size
    );
    println!("| max_buffer_size | {} |", limits.max_buffer_size);
}
