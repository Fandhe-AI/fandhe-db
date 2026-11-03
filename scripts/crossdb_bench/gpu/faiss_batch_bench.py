"""FAISS（CPU／GPU f32／GPU f16）のバッチ検索ベンチマーク。

RTX 3060 上の GPU 高速化を他実装（Rust 側 `crates/engine/benches/gpu_scaling_bench.rs`。
本スクリプトからは編集しないが、規模格子・k=10 を対応させる目的で参照する）と横並びに
比較するための crossdb_bench/gpu 配下スクリプトの一つ。docs/spec は参照しない。

本スクリプトは `bench-faiss-gpu` イメージ（Dockerfile.faiss。Python 3.12 +
`faiss-gpu-cu12==1.14.1.post1` + numpy。ホストの Python 3.14 には faiss-gpu の
wheel が無いため、コンテナ内でのみ実行する）のコンテナ内で実行する前提。

    docker build -t bench-faiss-gpu -f scripts/crossdb_bench/gpu/Dockerfile.faiss \
        scripts/crossdb_bench/gpu
    docker run --rm --gpus all -v "$S":/work bench-faiss-gpu \
        python /work/faiss_batch_bench.py --out /work/results/faiss.json

計測対象は 3 経路:
  (a) CPU  : `faiss.IndexFlatIP`（OMP スレッド数を明示指定。既定 12）
  (b) GPU f32: `faiss.index_cpu_to_gpu` で転送した f32 常駐 `GpuIndexFlatIP`
  (c) GPU f16: `GpuIndexFlatConfig.useFloat16=True`（ビルドに無ければ N/A を記録）

距離指標は自作 DB の `<=>`（内積）に合わせ内積（`IndexFlatIP`）を使う。
決定的合成データ（seed 固定・一様乱数 f32）で `rows × dim × batch` の格子を
総当たりし、各構成 warmup 5 回・計測 20 回の `search` 所要時間（マイクロ秒）を
集めて min/p50/p95/mean、および 1 クエリあたり p50（p50_us / batch）を出す。

GPU 側は `add`（データ転送・索引構築）を計測区間の外に置き、`search` 呼び出し
（FAISS の GPU search は呼び出し元スレッドから見て同期完了する API）のみを
計測する。転送コストは輸送経路の比較対象ではないため含めない（README 参照）。
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
import time
from typing import Any

import numpy as np

# rows / dim / batch の規模格子。gpu_scaling_bench.rs（Rust 側）の格子と対応させる
# ための本ハーネス独自の既定値（docs/spec 由来ではない）。
ROWS_GRID = (20_000, 100_000, 500_000)
DIM_GRID = (128, 256)
BATCH_GRID = (1, 8, 64, 256)
K = 10

WARMUP = 5
ITERS = 20

SEED = 20260905  # 決定性のための固定 seed（本ハーネス独自の既定値）。


def gen_data(rows: int, dim: int, n_queries: int, seed: int) -> tuple[np.ndarray, np.ndarray]:
    """一様乱数 f32 の決定的合成データを生成する（コーパス・クエリとも同一分布）。"""
    rng = np.random.default_rng(seed)
    corpus = rng.uniform(-1.0, 1.0, size=(rows, dim)).astype(np.float32)
    queries = rng.uniform(-1.0, 1.0, size=(n_queries, dim)).astype(np.float32)
    return corpus, queries


def percentile(sorted_values: list[float], p: float) -> float:
    if not sorted_values:
        return float("nan")
    n = len(sorted_values)
    idx = max(0, min(n - 1, int(round(p / 100.0 * (n - 1)))))
    return sorted_values[idx]


def latency_stats(latencies_us: list[float], batch: int) -> dict[str, Any]:
    arr = sorted(latencies_us)
    p50 = percentile(arr, 50)
    return {
        "n": len(arr),
        "min_us": arr[0],
        "p50_us": p50,
        "p95_us": percentile(arr, 95),
        "mean_us": sum(arr) / len(arr),
        "p50_us_per_query": p50 / batch if batch > 0 else float("nan"),
    }


def measure_search(index: Any, queries: np.ndarray, batch: int, k: int) -> dict[str, Any]:
    """`queries` を `batch` 件ずつのバッチに切り出し、warmup 後に計測する。

    queries の行数が batch 未満の場合は先頭から wrap して埋める（本ハーネス独自の
    決定的な取り回し。母集団分布は変えない）。
    """
    n = queries.shape[0]

    def batch_at(i: int) -> np.ndarray:
        start = (i * batch) % n
        idxs = [(start + j) % n for j in range(batch)]
        return queries[idxs]

    for i in range(WARMUP):
        index.search(batch_at(i), k)

    latencies_us = []
    for i in range(ITERS):
        b = batch_at(WARMUP + i)
        t0 = time.perf_counter()
        index.search(b, k)
        t1 = time.perf_counter()
        latencies_us.append((t1 - t0) * 1_000_000.0)

    return latency_stats(latencies_us, batch)


# FAISS 既定の `distance_compute_blas_threshold`（faiss.cvar の初期値。README 参照）。
FAISS_DEFAULT_BLAS_THRESHOLD = 20
# この値以上を「BLAS 経路無効化」とみなす（`--blas-threshold` の既定値 1<<30 と同じ）。
BLAS_DISABLED_THRESHOLD = 1 << 30


def cpu_condition_label(blas_threshold: int) -> str:
    """結果 JSON の `meta.cpu_condition` ラベル。README の比較条件の識別に使うため、
    FAISS 既定値（20）ちょうどの場合だけ `blas_default_threshold_20` とし、それ以外の
    任意値は `blas_custom_threshold_<n>` として既定条件と区別する（codex-review P2 指摘。
    例えば 1 を渡すと batch=1 から BLAS 経路になり既定 20 とは別条件）。"""
    if blas_threshold >= BLAS_DISABLED_THRESHOLD:
        return "blas_disabled"
    if blas_threshold == FAISS_DEFAULT_BLAS_THRESHOLD:
        return "blas_default_threshold_20"
    return f"blas_custom_threshold_{blas_threshold}"


def build_cpu_index(corpus: np.ndarray, dim: int, omp_threads: int, blas_threshold: int | None):
    """CPU 経路の `IndexFlatIP` を構築する。

    FAISS は `nq >= faiss.cvar.distance_compute_blas_threshold`（既定 20）で
    BLAS（sgemm）経路へ切り替わる。slim イメージでは OpenBLAS のスレッド数が
    既定でコア数いっぱいに張られ、OMP スレッドと競合して batch>=64 相当の
    クエリ数で大幅に悪化する事象を実測確認済み（README「CPU 経路の BLAS/OMP
    スレッド競合」節参照）。`blas_threshold` を大きくすると BLAS 経路を無効化し
    SIMD 直接計算（OMP スレッドのみ）に固定できる。
    """
    import faiss

    faiss.omp_set_num_threads(omp_threads)
    if blas_threshold is not None:
        faiss.cvar.distance_compute_blas_threshold = blas_threshold
    index = faiss.IndexFlatIP(dim)
    index.add(corpus)
    return index


def build_gpu_index(corpus: np.ndarray, dim: int, use_float16: bool):
    """GPU 常駐 `GpuIndexFlatIP` を構築する。`useFloat16` 未対応ビルドでは None を返す。

    `GpuIndexFlatIP(res, dim, config)` で直接 GPU 上に確保する（`index_cpu_to_gpu`
    は `GpuClonerOptions` を取る別 API のため、こちらは使わない）。
    """
    import faiss

    res = faiss.StandardGpuResources()
    config = faiss.GpuIndexFlatConfig()
    try:
        config.useFloat16 = use_float16
    except AttributeError:
        if use_float16:
            return None
    gpu_index = faiss.GpuIndexFlatIP(res, dim, config)
    gpu_index.add(corpus)
    return gpu_index


def faiss_meta() -> dict[str, Any]:
    import faiss

    return {
        "faiss_version": getattr(faiss, "__version__", "unknown"),
        "num_gpus": faiss.get_num_gpus(),
    }


def nvidia_smi_gpu_name() -> str | None:
    try:
        out = subprocess.run(
            ["nvidia-smi", "-L"], capture_output=True, text=True, timeout=5, check=False
        )
        if out.returncode == 0 and out.stdout.strip():
            return out.stdout.strip().splitlines()[0]
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None


def run_one(rows: int, dim: int, omp_threads: int, blas_threshold: int | None) -> dict[str, Any]:
    n_queries = 1024  # batch=256 まで wrap なしで賄える程度の決定的クエリプール件数。
    corpus, queries = gen_data(rows, dim, n_queries, seed=SEED + rows + dim)

    cpu_index = build_cpu_index(corpus, dim, omp_threads, blas_threshold)
    gpu_f32_index = build_gpu_index(corpus, dim, use_float16=False)
    gpu_f16_index = build_gpu_index(corpus, dim, use_float16=True)

    result: dict[str, Any] = {"rows": rows, "dim": dim, "k": K, "batches": {}}
    for batch in BATCH_GRID:
        entry: dict[str, Any] = {}
        entry["cpu"] = measure_search(cpu_index, queries, batch, K)
        entry["gpu_f32"] = measure_search(gpu_f32_index, queries, batch, K)
        entry["gpu_f16"] = (
            measure_search(gpu_f16_index, queries, batch, K) if gpu_f16_index is not None else "N/A"
        )
        result["batches"][str(batch)] = entry
        gpu_f16_p50 = (
            "N/A" if entry["gpu_f16"] == "N/A" else f"{entry['gpu_f16']['p50_us']:.1f}us"
        )
        print(
            f"  rows={rows} dim={dim} batch={batch}: "
            f"cpu_p50={entry['cpu']['p50_us']:.1f}us "
            f"gpu_f32_p50={entry['gpu_f32']['p50_us']:.1f}us "
            f"gpu_f16_p50={gpu_f16_p50}",
            file=sys.stderr,
        )

    del cpu_index, gpu_f32_index, gpu_f16_index
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, help="結果 JSON の出力先パス")
    parser.add_argument(
        "--omp-threads",
        type=int,
        default=int(os.environ.get("OMP_NUM_THREADS", "12")),
        help="FAISS CPU 経路の OpenMP スレッド数（既定 12。CLAUDE.md 依頼どおり明示指定）",
    )
    parser.add_argument(
        "--blas-threshold",
        type=int,
        default=int(os.environ.get("FAISS_BLAS_THRESHOLD", str(1 << 30))),
        help=(
            "faiss.cvar.distance_compute_blas_threshold の上書き値。既定 1<<30 で "
            "BLAS(sgemm) 経路を実質無効化し SIMD 直接計算に固定する（batch>=64 相当の "
            "nq で OpenBLAS スレッドと OMP スレッドが競合し大幅に悪化する事象への対策。"
            "README「CPU 経路の BLAS/OMP スレッド競合」節参照）。FAISS 既定の BLAS 経路を"
            "そのまま使いたい場合は 20 を指定する。"
        ),
    )
    parser.add_argument(
        "--rows",
        type=int,
        nargs="*",
        default=list(ROWS_GRID),
        help="規模格子の上書き（スモークテスト用。既定は %s）" % (ROWS_GRID,),
    )
    parser.add_argument("--dims", type=int, nargs="*", default=list(DIM_GRID))
    args = parser.parse_args()

    os.environ.setdefault("OMP_NUM_THREADS", str(args.omp_threads))

    meta: dict[str, Any] = {
        "generated_at_unix": time.time(),
        "python_version": platform.python_version(),
        "omp_num_threads": args.omp_threads,
        "openblas_num_threads_env": os.environ.get("OPENBLAS_NUM_THREADS"),
        "cpu_blas_threshold": args.blas_threshold,
        "cpu_condition": cpu_condition_label(args.blas_threshold),
        "seed": SEED,
        "k": K,
        "batch_grid": list(BATCH_GRID),
        "gpu_name": nvidia_smi_gpu_name(),
    }
    meta.update(faiss_meta())
    print(f"[faiss_batch_bench] meta={meta}", file=sys.stderr)

    results = []
    for rows in args.rows:
        for dim in args.dims:
            print(f"[faiss_batch_bench] running rows={rows} dim={dim} ...", file=sys.stderr)
            results.append(run_one(rows, dim, args.omp_threads, args.blas_threshold))

    out = {"meta": meta, "results": results}
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=2)
    print(f"[faiss_batch_bench] wrote {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
