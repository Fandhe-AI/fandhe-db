#!/usr/bin/env bash
# SIMD カーネル生成コード検査（`make simd-codegen-check` / `simd-codegen-check-cross` の実体）。
# check_simd_codegen.sh のセルフテストと本チェックを順に実行する。
# 使い方: scripts/simd_codegen_check.sh [cross]
#   cross: aarch64-unknown-linux-gnu 向け（要 target 導入。リンク不要）
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

case "${1:-}" in
    "")
        "${DIR}/check_simd_codegen.sh" --self-test
        "${DIR}/check_simd_codegen.sh"
        ;;
    cross)
        "${DIR}/check_simd_codegen.sh" --target aarch64-unknown-linux-gnu --self-test
        "${DIR}/check_simd_codegen.sh" --target aarch64-unknown-linux-gnu
        ;;
    *)
        echo "usage: simd_codegen_check.sh [cross]" >&2
        exit 2
        ;;
esac
