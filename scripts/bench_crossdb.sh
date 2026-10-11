#!/usr/bin/env bash
# self と対照 DB の機能別比較（`make bench-crossdb` の実体）。
# CROSSDB_DIR（fixture ディレクトリ）と CROSSDB_PYTHON（venv の python）が必須。
# 本体は scripts/crossdb_bench/run_all.sh。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

: "${CROSSDB_DIR:?CROSSDB_DIR must be set (fixture directory)}"
: "${CROSSDB_PYTHON:?CROSSDB_PYTHON must be set (venv python)}"
exec bash "${DIR}/crossdb_bench/run_all.sh"
