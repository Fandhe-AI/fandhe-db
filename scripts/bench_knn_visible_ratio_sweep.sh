#!/usr/bin/env bash
# Issue #487（可視比率〔1/2・1/4・1/10・1/20・1/50〕× 行数〔25k・100k〕での
# hnsw_subset と plain scan の損益分岐点計測）の交互計測ドライバ。
#
# `crates/engine/benches/knn_profile_bench.rs` の可視比率スイープ opt-in
# （`BENCH_KNN_PROFILE_VISIBLE_RATIO`／`BENCH_KNN_PROFILE_FULL_SCAN_RATIO`／
# `BENCH_KNN_PROFILE_SCALE`）を、scale × ratio の各組み合わせについて
# pair を外側・candidate を内側のループとして交互実行し、各 run の stdout を
# 個別ログへ保存する（計測規約 `docs/design/benchmark-judgement-policy.md`
# §3〜§5 の「交互 N≥5 ペア・per-run 生データ必須」に従う。ペアごとに
# baseline→cand1→baseline→cand2→baseline→cand3 の輪番を繰り返す構造にし、
# 特定の candidate だけを先に SWEEP_PAIRS 回連続実行してしまう時間方向の
# 交絡を避ける）。集計・表への転記は本スクリプトの責務外
# （実装者・運用者が `--summarize` で生成した一覧、または各ログを直接読んで行う）。
#
# 使い方: scripts/bench_knn_visible_ratio_sweep.sh [--summarize <dir>]
#   env SWEEP_PAIRS=<N>（既定 5。計測規約 §3 の N≥5 必須要件により 5 未満は拒否）
#   でペア数を上書きできる。
#
# candidate（brute_force を baseline として各 candidate の直前に必ず 1 回 baseline を
# 挟む輪番方式。計測規約 §3「3 候補以上の場合は baseline→cand1→baseline→cand2→…」
# に対応。baseline 単体を候補として扱わない——baseline は各候補の対照であり、
# 独立した「4 番目の候補」ではないため）:
#   - hnsw_default        : BENCH_KNN_PROFILE_ENGINE=hnsw（full_scan_ratio 既定 1/10）
#   - hnsw_force_ann       : 上記 ＋ BENCH_KNN_PROFILE_FULL_SCAN_RATIO=0/1（常に ANN 側）
#   - hnsw_force_plain      : 上記 ＋ BENCH_KNN_PROFILE_FULL_SCAN_RATIO=1/1（常に plain scan 側）
# baseline（各 candidate の直前に実行。BENCH_KNN_PROFILE_ENGINE=brute_force）
#
# `SWEEP_CANDIDATES=visited`（Issue #498）で candidate セットを
# `hnsw_force_ann_dense`（上記 hnsw_force_ann ＋
# BENCH_KNN_PROFILE_SPARSE_VISITED_MAX=0。既定＝常に dense）／
# `hnsw_force_ann_sparse`（上記 ＋
# BENCH_KNN_PROFILE_SPARSE_VISITED_MAX=18446744073709551615。常に sparse）の
# 2 candidate へ切り替えられる（`SWEEP_RATIOS`／`SWEEP_SCALES` で規模点も
# 上書き可）。`hnsw_force_ann_*` を使うのは、visited 実装の効果を可視候補数
# だけに帰属させたいため——`full_scan_ratio` 既定〔1/10〕のまま可視率を
# 動かすと ann_masked と plain scan の切替（Issue #487）が同時に交絡するため。
#
# `SWEEP_CANDIDATES=acorn`（ACORN-1〔2-hop 展開〕・Issue #501・#502）で
# candidate セットを `hnsw_one_hop`（既定・ACORN 無効。full_scan_ratio 既定
# 1/10）／`hnsw_acorn_1_1`（上記 ＋ BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO=
# 1/1。ACORN opt-in・full_scan_ratio 以上のあらゆる可視比率で TwoHop）の
# 2 candidate へ切り替えられる。`4/10`（`docs/design/hnsw-rls-cardinality-
# switch.md` の既定値候補）を独立 candidate としないのは、既定 5 比率
# （1/2・1/4・1/5・1/10・1/20）のもとでは各点の期待 arm が
# `hnsw_one_hop`／`hnsw_acorn_1_1` いずれかと完全に一致し（1/2 は
# full_scan_ratio=1/10 以上かつ 4/10 未満で 1-hop、1/4・1/5・1/10 は
# full_scan_ratio 以上かつ 4/10 以下で 2-hop、1/20 は full_scan_ratio 未満で
# plain scan——いずれも他方の candidate の観測値で代替できる）ため、3
# candidate 目を追加しても新規の測定点を得られないと判断したため
# （詳細は `docs/design/hnsw-rls-cardinality-switch.md`「Issue #502」節）。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ACORN-1（Issue #501・#502）の観測行（`arm expected=`／`acorn(delta)`）は
# 本 Issue で追加した任意項目であり、それ以前に保存されたログには一切
# 出現しない。`grep` は一致行が無いと exit 1 を返すため、`set -euo pipefail`
# 下ではそのまま呼ぶと既存ログの集計自体が失敗扱いになってしまう
# （codex-review 指摘）。一致行なし（exit 1）は正常終了として扱い、
# ファイル読み取りエラー等（exit 2 以上）は従来どおり失敗として扱う。
summarize_optional_grep() {
  local pattern="$1" sed_expr="$2" out status=0
  out="$(grep -H "${pattern}" "${DIR}"/*.log 2>&1)" || status=$?
  case "${status}" in
    0) printf '%s\n' "${out}" | sed -E "${sed_expr}" ;;
    1) : ;; # 一致行なし（任意項目のため許容）
    *)
      printf '%s\n' "${out}" >&2
      exit "${status}"
      ;;
  esac
}

if [ "${1:-}" = "--summarize" ]; then
  DIR="${2:?usage: $0 --summarize <dir>}"
  # 各ログの `stage(S0_hot_where_subset): ... median=<N>ms` 行を
  # scale/ratio/arm/pair ごとに一覧化する（表への転記は実装者が行う。
  # awk のみで完結させ追加依存を持たない）。
  grep -H "stage(S0_hot_where_subset)" "${DIR}"/*.log | \
    sed -E 's#.*/([^/]+)\.log:stage\(S0_hot_where_subset\): rows=([0-9]+) median=([0-9.]+)ms.*#\1 rows=\2 median=\3ms#'
  # ACORN-1（Issue #501・#502）の観測 arm・発火回数（`--summarize` 一覧に
  # レイテンシと並べて転記できるよう、同じログファイル名を先頭に付与する）。
  summarize_optional_grep "arm expected=" 's#.*/([^/]+)\.log:knn_profile_bench: (arm expected=.*)#\1 \2#'
  summarize_optional_grep "acorn(delta)" 's#.*/([^/]+)\.log:knn_profile_bench: (acorn\(delta\).*)#\1 \2#'
  exit 0
fi

if [ -n "${GITHUB_ACTIONS:-}" ]; then
  echo "ERROR: refusing to run under GITHUB_ACTIONS (CI 非配線・手動専用。Issue #487)" >&2
  exit 1
fi

SWEEP_PAIRS="${SWEEP_PAIRS:-5}"
if ! [[ "${SWEEP_PAIRS}" =~ ^[0-9]+$ ]] || [ "${SWEEP_PAIRS}" -lt 5 ]; then
  # 計測規約（docs/design/benchmark-judgement-policy.md §3）は交互 N≥5 ペアを
  # 必須事項として定めており「推奨」ではない。5 未満は新規計測として不可。
  echo "ERROR: SWEEP_PAIRS must be an integer >= 5 (benchmark-judgement-policy.md §3), got: ${SWEEP_PAIRS}" >&2
  exit 1
fi

TS="$(date +%s)"
OUT_DIR="${REPO_ROOT}/target/bench-knn-visible-ratio/${TS}"
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

echo "building knn_profile_bench (release, once)"
(cd "${REPO_ROOT}" && cargo bench --bench knn_profile_bench -p fandhe-db-engine --no-run)

DEFAULT_CANDIDATE_SET="default"
CANDIDATE_SET="${SWEEP_CANDIDATES:-${DEFAULT_CANDIDATE_SET}}"

case "${CANDIDATE_SET}" in
  default)
    DEFAULT_RATIOS=("1/2" "1/4" "1/10" "1/20" "1/50")
    DEFAULT_SCALES=(1 4)
    # baseline（brute_force）を含まない候補一覧。各候補の直前に必ず baseline を
    # 1 回実行する（輪番: baseline→cand1→baseline→cand2→baseline→cand3）。
    CANDIDATES=(hnsw_default hnsw_force_ann hnsw_force_plain)
    ;;
  visited)
    # Issue #498: visited 集合切替閾値の効果を可視候補数だけへ帰属させる
    # ため full_scan_ratio を常に 0/1（常に ANN 側）へ固定した 2 candidate。
    DEFAULT_RATIOS=("1/2" "1/10")
    DEFAULT_SCALES=(1)
    CANDIDATES=(hnsw_force_ann_dense hnsw_force_ann_sparse)
    ;;
  acorn)
    # Issue #501・#502: ACORN-1（2-hop 展開）opt-in の可視比率別 Recall・
    # レイテンシ前後比較。full_scan_ratio 既定（1/10）のまま可視比率を動かす
    # ため、1/20 は両 candidate とも plain scan（対照点）になる。
    DEFAULT_RATIOS=("1/2" "1/4" "1/5" "1/10" "1/20")
    DEFAULT_SCALES=(1)
    CANDIDATES=(hnsw_one_hop hnsw_acorn_1_1)
    ;;
  *)
    echo "ERROR: unknown SWEEP_CANDIDATES=${CANDIDATE_SET} (expected: default, visited, acorn)" >&2
    exit 1
    ;;
esac

if [ -n "${SWEEP_RATIOS:-}" ]; then
  read -r -a RATIOS <<<"${SWEEP_RATIOS}"
else
  RATIOS=("${DEFAULT_RATIOS[@]}")
fi
if [ -n "${SWEEP_SCALES:-}" ]; then
  read -r -a SCALES <<<"${SWEEP_SCALES}"
else
  SCALES=("${DEFAULT_SCALES[@]}")
fi

# arm ごとの env 設定を解決する（$1=arm 名。case 全分岐で
# BENCH_KNN_PROFILE_FULL_SCAN_RATIO／BENCH_KNN_PROFILE_SPARSE_VISITED_MAX／
# BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO を明示設定し、親シェルからの
# export 値が else 分岐で意図せず引き継がれる事故を防ぐ（Issue #502で
# ACORN_MAX_VISIBLE_RATIO を追加した際も同方針を踏襲——全分岐で明示）。
# 空文字列は harness 側で「未設定」＝既定値として扱われる。
resolve_env() {
  local arm="$1"
  SPARSE_VISITED_MAX=""
  ACORN_MAX_VISIBLE_RATIO=""
  case "${arm}" in
    baseline) ENGINE="brute_force"; FULL_SCAN_RATIO="" ;;
    hnsw_default) ENGINE="hnsw"; FULL_SCAN_RATIO="" ;;
    hnsw_force_ann) ENGINE="hnsw"; FULL_SCAN_RATIO="0/1" ;;
    hnsw_force_plain) ENGINE="hnsw"; FULL_SCAN_RATIO="1/1" ;;
    hnsw_force_ann_dense) ENGINE="hnsw"; FULL_SCAN_RATIO="0/1"; SPARSE_VISITED_MAX="0" ;;
    hnsw_force_ann_sparse) ENGINE="hnsw"; FULL_SCAN_RATIO="0/1"; SPARSE_VISITED_MAX="18446744073709551615" ;;
    hnsw_one_hop) ENGINE="hnsw"; FULL_SCAN_RATIO="" ;;
    hnsw_acorn_1_1) ENGINE="hnsw"; FULL_SCAN_RATIO=""; ACORN_MAX_VISIBLE_RATIO="1/1" ;;
    *) echo "ERROR: unknown arm ${arm}" >&2; exit 1 ;;
  esac
}

run_one() {
  local scale="$1" ratio="$2" arm="$3" label="$4" pair="$5"
  local ratio_slug="${ratio/\//_}"
  local log="${OUT_DIR}/scale${scale}_ratio${ratio_slug}_${label}_pair${pair}.log"
  local ENGINE FULL_SCAN_RATIO SPARSE_VISITED_MAX ACORN_MAX_VISIBLE_RATIO
  resolve_env "${arm}"

  echo "loadavg=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo n/a)" >"${log}"
  BENCH_KNN_PROFILE_VISIBLE_RATIO="${ratio}" \
    BENCH_KNN_PROFILE_ENGINE="${ENGINE}" \
    BENCH_KNN_PROFILE_SCALE="${scale}" \
    BENCH_KNN_PROFILE_FULL_SCAN_RATIO="${FULL_SCAN_RATIO}" \
    BENCH_KNN_PROFILE_SPARSE_VISITED_MAX="${SPARSE_VISITED_MAX}" \
    BENCH_KNN_PROFILE_ACORN_MAX_VISIBLE_RATIO="${ACORN_MAX_VISIBLE_RATIO}" \
    cargo bench --bench knn_profile_bench -p fandhe-db-engine >>"${log}" 2>&1
}

cd "${REPO_ROOT}"
for scale in "${SCALES[@]}"; do
  for ratio in "${RATIOS[@]}"; do
    # pair を外側、candidate を内側にする（計測規約 §3 の輪番
    # baseline→cand1→baseline→cand2→baseline→cand3 をペアごとに繰り返す。
    # candidate を外側にすると (baseline→候補1) を SWEEP_PAIRS 回連続
    # 実行してから候補2 に進む形になり、輪番にならず時間方向の交絡が
    # 生じる〔codex-review P1 指摘〕）。
    for pair in $(seq 1 "${SWEEP_PAIRS}"); do
      for candidate in "${CANDIDATES[@]}"; do
        echo "run: scale=${scale} ratio=${ratio} arm=baseline(for ${candidate}) pair=${pair}"
        run_one "${scale}" "${ratio}" "baseline" "baseline_for_${candidate}" "${pair}"
        echo "run: scale=${scale} ratio=${ratio} arm=${candidate} pair=${pair}"
        run_one "${scale}" "${ratio}" "${candidate}" "${candidate}" "${pair}"
      done
    done
  done
done

echo "done. logs in ${OUT_DIR}"
echo "summarize with: scripts/bench_knn_visible_ratio_sweep.sh --summarize ${OUT_DIR}"
