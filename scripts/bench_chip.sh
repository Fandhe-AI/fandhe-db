#!/usr/bin/env bash
# チップ別手動計測（`make bench-chip` の実体。Issue #469。手動専用）。
# 先に dot_kernel_bench・knn_profile_bench を --no-run でビルドし、feature_bench example を
# release ビルドしてから chip_bench を実行する。
set -euo pipefail

cargo bench --bench dot_kernel_bench -p fandhe-db-engine --no-run
cargo bench --bench knn_profile_bench -p fandhe-db-engine --no-run
cargo build --release -p fandhe-db-engine --example feature_bench
cargo bench --bench chip_bench -p fandhe-db-engine
