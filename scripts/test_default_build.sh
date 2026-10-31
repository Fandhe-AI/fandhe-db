#!/usr/bin/env bash
# 既定ビルド専用の回帰テスト（`make test-default-build` の実体。Issue #710・#715・#1926）。
# `make test` は --all-features のため、feature 無効時にだけ成立する次の性質を
# ピンポイントで実行する。
#   - wire-server: fault-injection 無効で起動フラグが拒否されること
#   - engine: wasm-udf 無効で WASM UDF の登録・LANGUAGE wasm が 0A000 になること
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

cd "${ROOT_DIR}"
cargo test -p fandhe-db-wire-server --test wire_fault_injection_cli
cargo test -p fandhe-db-engine --test wasm_udf_feature_off
