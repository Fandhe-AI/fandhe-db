#!/usr/bin/env bash
# scripts/eval のユニットテスト（`make test-eval` の実体。TASK-118。python3 が必要）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

have_cmd python3 || die "python3 not installed: required for the scripts/eval tests"
cd "${ROOT_DIR}"
python3 -m unittest discover scripts/eval/tests
