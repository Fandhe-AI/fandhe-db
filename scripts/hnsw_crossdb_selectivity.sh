#!/usr/bin/env bash
# 選択率 33% の到達分類・Recall 比較（`make hnsw-crossdb-selectivity` の実体。Issue #659）。
# CROSSDB_DIR（crossdb fixture ディレクトリ）が必須。
set -euo pipefail

: "${CROSSDB_DIR:?CROSSDB_DIR must be set (e.g. make hnsw-crossdb-selectivity CROSSDB_DIR=<dir>)}"
cargo test --release -p fandhe-db-engine --test hnsw_crossdb_selectivity -- --ignored --nocapture
