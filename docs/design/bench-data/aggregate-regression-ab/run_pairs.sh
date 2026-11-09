#!/usr/bin/env bash
# Issue #1491: 2 コミットの self exact 交互計測（ペア内の順番は常に A -> B で固定）。
# 使い方: BENCH_TARGET=<共有 CARGO_TARGET_DIR> run_pairs.sh <A_sha> <A_label> <B_sha> <B_label> <out_dir> [pairs]
# 各 arm は 1 回の計測ごとに、そのコミット自身のソースでビルドし fixture を作り直して計測する
# （../aggregate-regression-bisect/run_bisect_step.sh）。出力は <out_dir>/pair<N>-<label>/。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
A_SHA="${1:?A_sha}"; A_LABEL="${2:?A_label}"; B_SHA="${3:?B_sha}"; B_LABEL="${4:?B_label}"
OUT="${5:?out_dir}"; PAIRS="${6:-5}"
[[ "${PAIRS}" =~ ^[0-9]+$ ]] || { echo "invalid pairs" >&2; exit 2; }
[[ "${A_LABEL}" =~ ^[a-z0-9-]+$ && "${B_LABEL}" =~ ^[a-z0-9-]+$ ]] || { echo "invalid label" >&2; exit 2; }
DRIVER="${HERE}/../aggregate-regression-bisect/run_bisect_step.sh"
mkdir -p "${OUT}"
for i in $(seq 1 "${PAIRS}"); do
  echo "loadavg pair${i}: $(cat /proc/loadavg)" >> "${OUT}/loadavg.log"
  "${DRIVER}" "${A_SHA}" "${OUT}/pair${i}-${A_LABEL}" "${PORT:-15492}"
  "${DRIVER}" "${B_SHA}" "${OUT}/pair${i}-${B_LABEL}" "${PORT:-15492}"
done
