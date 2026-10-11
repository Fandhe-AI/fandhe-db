#!/usr/bin/env bash
# コア API シグネチャ差分検知（`make core-api-check` の実体）。
# check_core_api.sh のセルフテストと本チェックを順に実行する。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

"${DIR}/check_core_api.sh" --self-test
"${DIR}/check_core_api.sh"
