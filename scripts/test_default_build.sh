#!/usr/bin/env bash
# 既定ビルド専用の回帰テスト（`make test-default-build` の実体。Issue #710・#715・#1926）。
# `make test` は --all-features のため、feature 無効時にだけ成立する次の性質を
# ピンポイントで実行する。
#   - wire-server: fault-injection 無効で起動フラグが拒否されること
#   - engine: wasm-udf 無効で WASM UDF の登録・LANGUAGE wasm が 0A000 になること
#   - engine: cross-encoder 無効で cross_encoder_available() が false・ONNX 構築がファイルに触れず拒否されること（Issue #2144）
#   - engine: english-stemmer 無効で english_stemmer_available() が false・有効化要求が拒否されること、
#     既定ビルドの依存木（wire-server・engine）に rust-stemmers が現れないこと（Issue #3050）
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

cd "${ROOT_DIR}"
cargo test -p fandhe-db-wire-server --test wire_fault_injection_cli
cargo test -p fandhe-db-engine --test wasm_udf_feature_off
cargo test -p fandhe-db-engine --test cross_encoder_feature_off
cargo test -p fandhe-db-engine --test english_stemmer_feature_off

# 既定ビルドの依存木にステマー依存が現れないこと（opt-in feature の背後に閉じていること）。
for pkg in fandhe-db-wire-server fandhe-db-engine; do
  if cargo tree -p "${pkg}" -e normal --prefix none | grep -q '^rust-stemmers '; then
    echo "error: rust-stemmers appears in the default dependency tree of ${pkg}" >&2
    exit 1
  fi
done
