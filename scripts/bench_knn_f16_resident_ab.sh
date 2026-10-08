#!/usr/bin/env bash
# Issue #516（f16 常駐の前後比較〔25k／100k／500k 行・dim 128／768〕と常駐
# メモリの記録）の交互計測ドライバ。`crates/engine/benches/knn_profile_bench.rs`
# の hot-only モード（`BENCH_KNN_PROFILE_HOT_ONLY=1`）・索引単体メモリモード
# （`BENCH_KNN_PROFILE_INDEX_MEMORY=1`）を、`scripts/bench_knn_visible_ratio_sweep.sh`
# と同じ「pair を外側・candidate を内側」の輪番構造で実行する
# （計測規約 `docs/design/benchmark-judgement-policy.md` §3〜§5 の
# 「交互 N≥5 ペア・per-run 生データ必須」に従う）。
#
# 計測順序（要件 A〔前後比較〕を要件 B〔常駐メモリ〕より先に完走させる。
# 索引単体メモリモードは 500k×768 点で VmHWM 約 4.7 GiB に達するため、
# 先に実行すると OOM 等でプロセスが落ちた場合に主目的の A/B（hot-only
# レイテンシ）が一切測定されずに終わる恐れがある——`docs/design/
# hnsw-f16-resident.md` の受け入れ条件 1 が本 Issue の主目的のため）:
#   1. hot-only レイテンシ（25k dim128 → 100k dim128 → 500k dim128 →
#      25k dim768 → 100k dim768。各点で pair 回、baseline(brute_force)→
#      hnsw→baseline→<candidate>の輪番。candidate が複数〔Issue #526〕の
#      場合は baseline→hnsw→baseline→cand1→baseline→cand2→… と続く）
#   2. 索引単体メモリモード（5 SQL 到達可能点 × {hnsw, <candidates...>} × 2 回）
#   3. 500k×768 の索引単体メモリのみ（SQL 表層は arena 1 GiB 上限で
#      構造的に到達不能。`docs/design/hnsw-f16-resident.md` 参照）
#
# Issue #523 で `AB_CANDIDATE_ENGINE`（既定 `hnsw_f16`。`hnsw_i8` も受理する
# 許可リスト opt-in）を追加し、hnsw との前後比較対象を選べるよう一般化した
# （既定は従来どおり f16 のまま出力ディレクトリ・挙動とも不変）。
#
# Issue #526（Apple M 実機での i8／f16／f32 経路の前後比較）で
# `AB_CANDIDATE_ENGINES`（空白区切りの複数候補。許可リスト `hnsw_f16`／
# `hnsw_i8` の各トークンを個別検証・重複拒否）を追加し、同一計測セッション
# （同一ノイズ帯）で 3 精度（f32〔hnsw〕・f16・i8）を一括計測できるように
# 一般化した。`AB_CANDIDATE_ENGINE`（単数）・`AB_CANDIDATE_ENGINES`（複数）は
# 両方同時に指定すると fail-closed で拒否する（値をそのままログファイル名・
# ディレクトリ名へ使うため、許可リスト検証を最初に行う設計は変わらない）。
#
# 使い方: scripts/bench_knn_f16_resident_ab.sh [--summarize <dir>]
#   env AB_PAIRS=<N>（既定 5。5 未満は拒否）
#   env AB_POINTS="scale:dim scale:dim ..."（既定 "1:128 4:128 20:128 1:768 4:768"）
#   env AB_MEMORY_POINTS="scale:dim ..."（既定 AB_POINTS ＋ "20:768"）
#   env AB_CANDIDATE_ENGINE=<hnsw_f16|hnsw_i8>（既定 hnsw_f16。単一候補。
#     許可リスト以外は拒否。AB_CANDIDATE_ENGINES と同時指定は拒否）
#   env AB_CANDIDATE_ENGINES="hnsw_f16 hnsw_i8"（複数候補。空白区切り。
#     許可リスト外・重複トークンは拒否。指定時は出力ディレクトリが
#     target/bench-knn-precision-resident/<ts>/ に固定される）
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  DIR="${2:?usage: $0 --summarize <dir>}"
  # per-run ログから測定行を 1 行 1 レコードの TSV へ集約する（生データは
  # ログとして保持し続ける。PR #580 の codex-review 指摘の慣行を踏襲）。
  # `kernel_isa`（Issue #526。非 vacuous 証跡: 実行時ディスパッチされた
  # f32/f16/i8 カーネルの ISA）も本 TSV の対象に含める。
  {
    printf 'file\tkind\tfields\n'
    grep -H -E "stage\(S0_hot_sql_e2e\)|stage\(S0prime_count_star\)|^raw\(|hnsw_stats |resident_precision |index_memory |index_warm_ms|kernel_isa " \
      "${DIR}"/*.log 2>/dev/null | \
      sed -E 's#^([^:]+):#\1\t#' | \
      awk -F'\t' '{
        rest=$2
        kind="other"
        if (rest ~ /^stage\(/) kind="stage"
        else if (rest ~ /^raw\(/) kind="raw"
        else if (rest ~ /hnsw_stats /) kind="hnsw_stats"
        else if (rest ~ /resident_precision /) kind="resident_precision"
        else if (rest ~ /index_memory /) kind="index_memory"
        else if (rest ~ /index_warm_ms=/) kind="index_warm_ms"
        else if (rest ~ /kernel_isa /) kind="kernel_isa"
        printf "%s\t%s\t%s\n", $1, kind, rest
      }'
  }
  exit 0
fi

if [ -n "${GITHUB_ACTIONS:-}" ]; then
  echo "ERROR: refusing to run under GITHUB_ACTIONS (CI 非配線・手動専用。Issue #516)" >&2
  exit 1
fi

AB_PAIRS="${AB_PAIRS:-5}"
if ! [[ "${AB_PAIRS}" =~ ^[0-9]+$ ]] || [ "${AB_PAIRS}" -lt 5 ]; then
  echo "ERROR: AB_PAIRS must be an integer >= 5 (benchmark-judgement-policy.md §3), got: ${AB_PAIRS}" >&2
  exit 1
fi

# `AB_CANDIDATE_ENGINE`（単数・従来経路）と `AB_CANDIDATE_ENGINES`（複数・
# Issue #526）は片方のみを受理する。両方を env に明示指定した場合は
# 値をそのまま信用せず fail-closed で拒否する（`${VAR+x}` は未設定と
# 空文字列を区別する bash のパラメータ展開。値の中身を見る前に検証する）。
if [ -n "${AB_CANDIDATE_ENGINE+x}" ] && [ -n "${AB_CANDIDATE_ENGINES+x}" ]; then
  echo "ERROR: set only one of AB_CANDIDATE_ENGINE or AB_CANDIDATE_ENGINES, not both" >&2
  exit 1
fi

# 許可リスト検証（単一トークン）。値をログファイル名・ディレクトリ名へ
# そのまま使うため、任意文字列の混入を防ぐ意味でも最初に固定する。
validate_candidate_token() {
  case "$1" in
    hnsw_f16|hnsw_i8) return 0 ;;
    *) return 1 ;;
  esac
}

MULTI_CANDIDATE_MODE=0
if [ -n "${AB_CANDIDATE_ENGINES+x}" ]; then
  MULTI_CANDIDATE_MODE=1
  read -r -a AB_CANDIDATES_ARR <<<"${AB_CANDIDATE_ENGINES}"
  if [ "${#AB_CANDIDATES_ARR[@]}" -eq 0 ]; then
    echo "ERROR: AB_CANDIDATE_ENGINES must list at least one candidate" >&2
    exit 1
  fi
  # 重複検出は連想配列（`declare -A`）を使わず、区切り文字付き文字列への
  # 部分一致で行う（macOS 標準 `/bin/bash` 3.2 は連想配列未対応で
  # `declare -A` が `invalid option` エラーとなり `set -e` で計測前に
  # 停止するため。候補は許可リスト検証済みトークンのみで区切り文字
  # `|` を含み得ないため、部分一致による誤検出は起きない）。
  SEEN_CANDIDATES_STR="|"
  for cand in "${AB_CANDIDATES_ARR[@]}"; do
    if ! validate_candidate_token "${cand}"; then
      echo "ERROR: AB_CANDIDATE_ENGINES entries must be \"hnsw_f16\" or \"hnsw_i8\", got: ${cand}" >&2
      exit 1
    fi
    case "${SEEN_CANDIDATES_STR}" in
      *"|${cand}|"*)
        echo "ERROR: AB_CANDIDATE_ENGINES contains duplicate entry: ${cand}" >&2
        exit 1
        ;;
    esac
    SEEN_CANDIDATES_STR="${SEEN_CANDIDATES_STR}${cand}|"
  done
else
  AB_CANDIDATE_ENGINE="${AB_CANDIDATE_ENGINE:-hnsw_f16}"
  if ! validate_candidate_token "${AB_CANDIDATE_ENGINE}"; then
    echo "ERROR: AB_CANDIDATE_ENGINE must be \"hnsw_f16\" or \"hnsw_i8\", got: ${AB_CANDIDATE_ENGINE}" >&2
    exit 1
  fi
  AB_CANDIDATES_ARR=("${AB_CANDIDATE_ENGINE}")
fi

# 出力ディレクトリ名（単一候補時は候補ごとの短縮ラベルを使い既存
# docs・Makefile 参照との互換を保つ。複数候補時は固定ラベルへ切り替える
# ——env 値をそのままパスへ埋め込まない現行方針〔許可リスト検証済み
# トークンのみを使う〕を維持する）。
if [ "${MULTI_CANDIDATE_MODE}" -eq 1 ]; then
  CANDIDATE_DIR_LABEL="precision"
else
  case "${AB_CANDIDATE_ENGINE}" in
    hnsw_f16) CANDIDATE_DIR_LABEL="f16" ;;
    hnsw_i8) CANDIDATE_DIR_LABEL="i8" ;;
  esac
fi

# デフォルトの規模点: 25k/100k/500k × dim128、25k/100k × dim768
# （500k×768 は SQL 表層〔hot-only〕では arena 1 GiB 上限〔MAX_ARENA_TOTAL_BYTES〕
# により構造的に到達不能。`docs/design/hnsw-f16-resident.md` 参照）。
DEFAULT_POINTS="1:128 4:128 20:128 1:768 4:768"
read -r -a AB_POINTS_ARR <<<"${AB_POINTS:-${DEFAULT_POINTS}}"
# AB_MEMORY_POINTS の既定は「AB_POINTS ＋ 20:768」（AB_POINTS を上書きした
# 場合はそれに追随する。DEFAULT_POINTS 固定ではない）。
read -r -a AB_MEMORY_POINTS_ARR <<<"${AB_MEMORY_POINTS:-${AB_POINTS_ARR[*]} 20:768}"

TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="${REPO_ROOT}/target/bench-knn-${CANDIDATE_DIR_LABEL}-resident/${TS}"
mkdir -p "${OUT_DIR}"

# macOS 実機（Issue #526）向けの CPU 情報 best-effort 収集
# （`crates/engine/benches/chip_bench.rs::collect_cpu_info` と同じキー
# 集合。`/proc/cpuinfo` が無い環境でのみ使う fallback で、失敗しても
# 計測本体は止めない）。
collect_env_cpu_lines() {
  if [ -r /proc/cpuinfo ]; then
    echo "cpu_model=$(grep -m1 '^model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
    echo "cpu_flags=$(grep -m1 '^flags' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
    return 0
  fi
  if command -v sysctl >/dev/null 2>&1; then
    echo "cpu_brand_string=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unavailable)"
    echo "hw_perflevel0_physicalcpu=$(sysctl -n hw.perflevel0.physicalcpu 2>/dev/null || echo unavailable)"
    echo "hw_perflevel1_physicalcpu=$(sysctl -n hw.perflevel1.physicalcpu 2>/dev/null || echo unavailable)"
    echo "hw_optional_arm_FEAT_FP16=$(sysctl -n hw.optional.arm.FEAT_FP16 2>/dev/null || echo unavailable)"
    echo "hw_optional_arm_FEAT_DotProd=$(sysctl -n hw.optional.arm.FEAT_DotProd 2>/dev/null || echo unavailable)"
    echo "hw_optional_arm_FEAT_BF16=$(sysctl -n hw.optional.arm.FEAT_BF16 2>/dev/null || echo unavailable)"
    echo "hw_optional_arm_FEAT_I8MM=$(sysctl -n hw.optional.arm.FEAT_I8MM 2>/dev/null || echo unavailable)"
    echo "hw_optional_arm_FEAT_SME=$(sysctl -n hw.optional.arm.FEAT_SME 2>/dev/null || echo unavailable)"
  fi
  if command -v sw_vers >/dev/null 2>&1; then
    echo "sw_vers=$(sw_vers -productVersion 2>/dev/null || echo unavailable)"
  fi
  echo "uname_m=$(uname -m 2>/dev/null || echo unavailable)"
}

{
  echo "commit=$(cd "${REPO_ROOT}" && git rev-parse HEAD)"
  echo "nproc=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
  echo "BENCH_DEDICATED_ENV=${BENCH_DEDICATED_ENV:-<unset>}"
  collect_env_cpu_lines
  echo "rustc_version=$(rustc --version 2>/dev/null || echo unavailable)"
  echo "ab_pairs=${AB_PAIRS}"
  echo "ab_points=${AB_POINTS_ARR[*]}"
  echo "ab_memory_points=${AB_MEMORY_POINTS_ARR[*]}"
  if [ "${MULTI_CANDIDATE_MODE}" -eq 1 ]; then
    echo "ab_candidate_engines=${AB_CANDIDATES_ARR[*]}"
  else
    echo "ab_candidate_engine=${AB_CANDIDATE_ENGINE}"
  fi
} >"${OUT_DIR}/env.txt"

# 以降のすべての `cargo bench` 呼び出しをリポジトリルートから実行する
# （`scripts/bench_knn_visible_ratio_sweep.sh` と同型。本スクリプトを
# 別ディレクトリ・別 CWD から絶対パスで起動しても Cargo.toml を確実に
# 見つけられるようにするため、以降の全 cargo 呼び出しより前に置く）。
cd "${REPO_ROOT}"

echo "building knn_profile_bench (release, once)"
cargo bench --bench knn_profile_bench -p fandhe-db-engine --no-run

log_noise() {
  local log="$1"
  if [ -r /proc/loadavg ]; then
    echo "loadavg=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo n/a)" >"${log}"
    return 0
  fi
  if command -v sysctl >/dev/null 2>&1; then
    echo "loadavg=$(sysctl -n vm.loadavg 2>/dev/null || echo n/a)" >"${log}"
    return 0
  fi
  echo "loadavg=n/a" >"${log}"
}

# --- 1. hot-only レイテンシ（交互 N≥5 ペア）---------------------------------
# `slot` はログファイル名の一意化のみに使う（同一 pair 内で brute_force を
# 複数回計測するため、`engine` だけではファイル名が衝突し `log_noise` の
# 上書き〔`>`〕で先の計測が失われる。2026-09-07 の初回計測でこの衝突が
# 発生し、`hot_*_brute_force_pair*.log` は「直前の candidate の直前」の
# 1 回分しか残らなかった。前後比較の主対象は hnsw/<candidates> であり
# brute_force は補助系列のため、既存ログはそのまま「pair あたり・candidate
# 直前ごとに 1 回」の値として扱い、本修正後の新規計測も同じ位置
# 〔before<candidate token>〕で揃える。詳細は `docs/design/
# hnsw-f16-resident.md`「Issue #516 追記」節参照）。
run_hot_only() {
  local scale="$1" dim="$2" engine="$3" pair="$4" slot="$5"
  local log="${OUT_DIR}/hot_scale${scale}_dim${dim}_${engine}${slot}_pair${pair}.log"
  log_noise "${log}"
  BENCH_KNN_PROFILE_HOT_ONLY=1 \
    BENCH_KNN_PROFILE_ENGINE="${engine}" \
    BENCH_KNN_PROFILE_SCALE="${scale}" \
    BENCH_KNN_PROFILE_DIM="${dim}" \
    cargo bench --bench knn_profile_bench -p fandhe-db-engine >>"${log}" 2>&1
}

for point in "${AB_POINTS_ARR[@]}"; do
  scale="${point%%:*}"
  dim="${point##*:}"
  for pair in $(seq 1 "${AB_PAIRS}"); do
    echo "run: hot_only scale=${scale} dim=${dim} arm=brute_force(before hnsw) pair=${pair}"
    run_hot_only "${scale}" "${dim}" "brute_force" "${pair}" "_beforehnsw"
    echo "run: hot_only scale=${scale} dim=${dim} arm=hnsw pair=${pair}"
    run_hot_only "${scale}" "${dim}" "hnsw" "${pair}" ""
    for cand in "${AB_CANDIDATES_ARR[@]}"; do
      echo "run: hot_only scale=${scale} dim=${dim} arm=brute_force(before ${cand}) pair=${pair}"
      run_hot_only "${scale}" "${dim}" "brute_force" "${pair}" "_before${cand}"
      echo "run: hot_only scale=${scale} dim=${dim} arm=${cand} pair=${pair}"
      run_hot_only "${scale}" "${dim}" "${cand}" "${pair}" ""
    done
  done
done

# --- 2. 索引単体メモリモード（500k×768 点で VmHWM 約 4.7 GiB。上記 1 の
# 完走後に実行する——順序の理由は本ファイル冒頭コメント参照）。-------------
for point in "${AB_MEMORY_POINTS_ARR[@]}"; do
  scale="${point%%:*}"
  dim="${point##*:}"
  for engine in hnsw "${AB_CANDIDATES_ARR[@]}"; do
    for rep in 1 2; do
      log="${OUT_DIR}/mem_scale${scale}_dim${dim}_${engine}_rep${rep}.log"
      echo "run: memory scale=${scale} dim=${dim} engine=${engine} rep=${rep}"
      log_noise "${log}"
      BENCH_KNN_PROFILE_INDEX_MEMORY=1 \
        BENCH_KNN_PROFILE_ENGINE="${engine}" \
        BENCH_KNN_PROFILE_SCALE="${scale}" \
        BENCH_KNN_PROFILE_DIM="${dim}" \
        cargo bench --bench knn_profile_bench -p fandhe-db-engine >>"${log}" 2>&1
    done
  done
done

echo "done. logs in ${OUT_DIR}"
echo "summarize with: scripts/bench_knn_f16_resident_ab.sh --summarize ${OUT_DIR}"
