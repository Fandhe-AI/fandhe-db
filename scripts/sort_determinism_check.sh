#!/usr/bin/env bash
# ソート非決定性 API 再混入検知（`make sort-determinism-check` の実体）。
# check_sort_determinism.sh のセルフテストと本チェックを順に実行する。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

"${DIR}/check_sort_determinism.sh" --self-test
"${DIR}/check_sort_determinism.sh"
