#!/usr/bin/env bash
# Issue #498（visited 集合切替閾値 `sparse_visited_max` の可視比率スイープ
# 前後比較と既定値確定）の層 1 交互計測ドライバ。
#
# `crates/engine/benches/hnsw_search_bench.rs` へ Issue #491 の 1 規模点計測
# 基盤の上に追加した `BENCH_HNSW_SEARCH_SPARSE_VISITED_MAX` knob（dense=0／
# sparse=usize::MAX の 2 arm。`--features bench-internals` 限定の非 vacuous
# 検証つき）を、規模点（rows）× 可視率（mask）ごとに dense→sparse の交互
# ペアで実行し、各 run の stdout を個別ログへ保存する（計測規約
# `docs/design/benchmark-judgement-policy.md` §3〜§5 の「交互 N≥5 ペア・
# per-run 生データ必須」に従う）。集計・判定・表への転記は本スクリプトの
# 責務外（`--summarize` で生成した一覧、または各ログを直接読んで行う）。
#
# 使い方: scripts/bench_hnsw_search_visited_ab.sh [--summarize <dir>]
#   env AB_PAIRS=<N>（既定 5。計測規約 §3 の N≥5 必須要件により 5 未満は拒否）
#   env AB_ROWS="10000 100000"（既定。1 プロセス = 1 規模点。Issue #313）
#   env AB_MASKS="50 25 10 5 2"（既定。可視率%。`BENCH_HNSW_SEARCH_MASK` へ渡す）
#   でペア数・規模点・可視率を上書きできる。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  DIR="${2:?usage: $0 --summarize <dir>}"
  # 各ログの `target=`／`reference=`／`arm=`（visited_kind 行）を実行順
  # （rows→mask→pair→arm）に列挙するだけで判定はしない
  # （`bench_hybrid_profile_ab.sh` と同方針）。
  for log in "${DIR}"/*.log; do
    [ -e "${log}" ] || continue
    name="$(basename "${log}" .log)"
    grep -H "^hnsw_search_bench: target=\|^hnsw_search_bench: reference=\|^hnsw_search_bench: arm=" "${log}" | \
      sed "s#^#${name}: #"
  done
  exit 0
fi

if [ -n "${GITHUB_ACTIONS:-}" ]; then
  echo "ERROR: refusing to run under GITHUB_ACTIONS (CI 非配線・手動専用。Issue #498)" >&2
  exit 1
fi

AB_PAIRS="${AB_PAIRS:-5}"
if ! [[ "${AB_PAIRS}" =~ ^[0-9]+$ ]] || [ "${AB_PAIRS}" -lt 5 ]; then
  # 計測規約（docs/design/benchmark-judgement-policy.md §3）は交互 N≥5 ペアを
  # 必須事項として定めており「推奨」ではない。5 未満は新規計測として不可。
  echo "ERROR: AB_PAIRS must be an integer >= 5 (benchmark-judgement-policy.md §3), got: ${AB_PAIRS}" >&2
  exit 1
fi

read -r -a AB_ROWS_ARR <<<"${AB_ROWS:-10000 100000}"
read -r -a AB_MASKS_ARR <<<"${AB_MASKS:-50 25 10 5 2}"

TS="$(date +%s)"
OUT_DIR="${REPO_ROOT}/target/bench-hnsw-search-visited/${TS}"
mkdir -p "${OUT_DIR}"

{
  echo "commit=$(cd "${REPO_ROOT}" && git rev-parse HEAD)"
  echo "nproc=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
  echo "BENCH_DEDICATED_ENV=${BENCH_DEDICATED_ENV:-<unset>}"
  if [ -r /proc/cpuinfo ]; then
    echo "cpu_model=$(grep -m1 '^model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
    echo "cpu_flags=$(grep -m1 '^flags' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
  fi
} >"${OUT_DIR}/env.txt"

echo "building hnsw_search_bench (release, once, --features bench-internals)"
(cd "${REPO_ROOT}" && BENCH_HNSW_SEARCH_COMMIT="$(git rev-parse HEAD)" \
  cargo bench --bench hnsw_search_bench -p fandhe-db-engine --features bench-internals --no-run)

# arm ごとの env 設定を解決する（$1=arm 名。case 全分岐で明示設定し、親
# シェルからの export 値が意図せず引き継がれる事故を防ぐ）。
resolve_env() {
  local arm="$1"
  case "${arm}" in
    dense) SPARSE_VISITED_MAX="0" ;;
    sparse) SPARSE_VISITED_MAX="18446744073709551615" ;;
    *) echo "ERROR: unknown arm ${arm}" >&2; exit 1 ;;
  esac
}

run_one() {
  local rows="$1" mask="$2" arm="$3" pair="$4"
  local log="${OUT_DIR}/rows${rows}_mask${mask}_${arm}_pair${pair}.log"
  local SPARSE_VISITED_MAX
  resolve_env "${arm}"

  {
    echo "loadavg=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo n/a)"
  } >"${log}"
  BENCH_HNSW_SEARCH_ROWS="${rows}" \
    BENCH_HNSW_SEARCH_MASK="${mask}" \
    BENCH_HNSW_SEARCH_SPARSE_VISITED_MAX="${SPARSE_VISITED_MAX}" \
    BENCH_HNSW_SEARCH_COMMIT="$(cd "${REPO_ROOT}" && git rev-parse HEAD)" \
    BENCH_DEDICATED_ENV="${BENCH_DEDICATED_ENV:-}" \
    cargo bench --bench hnsw_search_bench -p fandhe-db-engine --features bench-internals >>"${log}" 2>&1
}

cd "${REPO_ROOT}"
for rows in "${AB_ROWS_ARR[@]}"; do
  for mask in "${AB_MASKS_ARR[@]}"; do
    # pair を外側、arm（dense→sparse）を内側にする輪番（計測規約 §3 の
    # 交互実行に対応。特定 arm を AB_PAIRS 回連続実行する時間方向の交絡を
    # 避ける）。
    for pair in $(seq 1 "${AB_PAIRS}"); do
      echo "run: rows=${rows} mask=${mask} arm=dense pair=${pair}"
      run_one "${rows}" "${mask}" "dense" "${pair}"
      echo "run: rows=${rows} mask=${mask} arm=sparse pair=${pair}"
      run_one "${rows}" "${mask}" "sparse" "${pair}"
    done
  done
done

echo "done. logs in ${OUT_DIR}"
echo "summarize with: scripts/bench_hnsw_search_visited_ab.sh --summarize ${OUT_DIR}"
