#!/usr/bin/env bash
# `crates/engine/benches/dot_kernel_bench.rs`（Issue #365・#512・手動専用の
# 情報提供ベンチ）を before/after（＋任意の追加候補）2 本以上のバイナリで
# 交互 min-of-N 実行するための薄いドライバ。Issue #519（親 #402・前提 #518）
# が要求する「dim 768／1536 の #518 前後比較・閾値候補の輪番実測」を
# `docs/design/benchmark-judgement-policy.md` の交互実行・per-run 生データ
# 保持・参照区間ノイズ帯規約に沿って行うために追加した。`scripts/
# bench_gpu_scaling_ab.sh` と同型の骨格（入力検証・生ログ保持・summary.tsv
# 追記）を踏襲する。
#
# 呼び出し元は人間の運用者（ベンチ本体が手動専用・CI 非配線のため `make`
# ターゲットは設けない）。呼び出し先は `BEFORE_BIN`/`AFTER_BIN`（および任意の
# `CAND_BINS`）として渡す `dot_kernel_bench` 実行ファイル（`cargo bench
# --bench dot_kernel_bench -p fandhe-db-engine --no-run` で得た成果物を退避したもの）。
# production コード（`crates/engine/src/`）・既存ベンチハーネスは一切変更しない。
#
# `dot_kernel_bench` は起動ごとに `DIMS=[100,128,384,768,1536]` ×
# `WorkingSet::{CacheResident,ArenaScale}` の全点を測るため、本スクリプトに
# 規模点パラメータは無い（`gpu_scaling_bench` の POINTS とは異なる）。
# `BENCH_DOT_KERNEL_BLOCK_AB=1` を常に子プロセスへ渡し、`BLOCK_AB_DIMS`
# （128・768）の block4 A/B・参照区間も同時に採取する。
#
# 使い方:
#   BEFORE_BIN=<path> AFTER_BIN=<path> [CAND_BINS="label1=<path> label2=<path>"] \
#     scripts/bench_dot_kernel_ab.sh [PAIRS]
#   PAIRS: 交互実行するペア数（既定 5・正整数のみ）。
#   CAND_BINS: 追加候補バイナリ（例: 閾値候補ビルド）。各要素は
#     "label=path" 形式。label は英数字・アンダースコアのみ。輪番順序は
#     policy.md §3 のとおり before → after → before → cand1 → before → cand2 … 。
#   OUT_DIR: 出力先（既定 _/bench/dot-kernel-ab/<UTC timestamp>）。
#
# 出力: <OUT_DIR>/<pair>-<side>.log に事前環境スナップショット・stdout 全文、
# <OUT_DIR>/summary.tsv に集計用の 1 行 1 計測点の TSV を追記する。
#
# 記録の保持（`bench_gpu_scaling_ab.sh` と同方針）: OUT_DIR の既定値は
# `.gitignore` 対象（`_/`）配下のため、結果を残す場合は summary.tsv を
# `docs/design/bench-data/dot-kernel-multi-acc-ab/<UTC timestamp>-summary.tsv`
# へ明示的にコピーし、対応する `docs/design/dot-kernel-multi-accumulator.md`
# の実測表からそのパスを参照すること（benchmark-judgement-policy.md §3）。
# 生ログ全文（*.log）まで tracked にする必要はない。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

: "${BEFORE_BIN:?BEFORE_BIN must be set to the before dot_kernel_bench binary path}"
: "${AFTER_BIN:?AFTER_BIN must be set to the after dot_kernel_bench binary path}"

[[ -x "${BEFORE_BIN}" ]] || die "BEFORE_BIN is not an executable file: ${BEFORE_BIN}"
[[ -x "${AFTER_BIN}" ]] || die "AFTER_BIN is not an executable file: ${AFTER_BIN}"

PAIRS="${1:-5}"
if ! [[ "${PAIRS}" =~ ^[0-9]+$ ]] || [ "${PAIRS}" -lt 1 ]; then
  die "PAIRS must be a positive integer, got: ${PAIRS}"
fi

# CAND_BINS は "label=path label2=path2 ..." の空白区切り。label はシェル・
# TSV へそのまま書き込むため許可文字集合で fail-closed に検証する
# （coding-rust.md「untrusted 入力の扱い」: env/引数インジェクション防止）。
# label は summary.tsv の side 列・per-pair ログファイル名（<pair>-<side>.log）
# へそのまま使われるため、`before`／`after`（大小無視の同名重複を含む）は
# 予約名として拒否し、CAND_BINS 内の重複も拒否する（重複・予約名を許すと
# `run_one` のログ衝突検出（既存ログを refuse-to-overwrite）に途中で
# 引っかかり、輪番の途中で N ペア完走できずに停止する）。
CAND_LABELS=()
CAND_PATHS=()
if [ -n "${CAND_BINS:-}" ]; then
  for entry in ${CAND_BINS}; do
    if [[ "${entry}" != *"="* ]]; then
      die "CAND_BINS entry must be label=path, got: ${entry}"
    fi
    label="${entry%%=*}"
    path="${entry#*=}"
    [[ "${label}" =~ ^[A-Za-z0-9_]+$ ]] || die "CAND_BINS label must match ^[A-Za-z0-9_]+\$, got: ${label}"
    label_lower="$(printf '%s' "${label}" | tr '[:upper:]' '[:lower:]')"
    if [[ "${label_lower}" == "before" || "${label_lower}" == "after" ]]; then
      die "CAND_BINS label must not reuse the reserved before/after names, got: ${label}"
    fi
    for existing in "${CAND_LABELS[@]:-}"; do
      [[ -z "${existing}" ]] && continue
      existing_lower="$(printf '%s' "${existing}" | tr '[:upper:]' '[:lower:]')"
      if [[ "${label_lower}" == "${existing_lower}" ]]; then
        die "CAND_BINS label must be unique, duplicate: ${label}"
      fi
    done
    [[ -x "${path}" ]] || die "CAND_BINS path is not an executable file: ${path}"
    CAND_LABELS+=("${label}")
    CAND_PATHS+=("${path}")
  done
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_OUT_DIR="${REPO_ROOT}/_/bench/dot-kernel-ab/$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="${OUT_DIR:-${DEFAULT_OUT_DIR}}"
mkdir -p "${OUT_DIR}"

SUMMARY="${OUT_DIR}/summary.tsv"
if [ ! -f "${SUMMARY}" ]; then
  # kind: current（label=current 行。1 行版 dot の #518 効果）／
  # block_single（block4_ab 行の single_row_median_ms）／
  # block4（block4_ab 行の block4_median_ms・ratio 列に B/A 比を持つ）／
  # block_ref（block4_ab_ref 行。ratio 列にプロセス内反復間ノイズ band を
  # 転用。run-to-run 幅は複数 run の ref_median_ms から呼び出し側〔doc〕が
  # 別途算出する）。
  printf 'side\tpair\tkind\tworking_set\tdim\trows\tvalue_ms\tns_per_dot\tratio\tstatus\n' > "${SUMMARY}"
fi

run_one() {
  local bin="$1" side="$2" pair="$3"
  local log="${OUT_DIR}/${pair}-${side}.log"

  if [ -e "${log}" ]; then
    die "raw log already exists (refusing to overwrite): ${log}. Use a fresh OUT_DIR for a new run."
  fi

  {
    echo "# pre-run environment snapshot"
    echo "loadavg: $(cat /proc/loadavg 2>/dev/null || echo unavailable)"
    echo "# dot_kernel_bench output"
  } > "${log}"

  local status=0
  BENCH_DOT_KERNEL_BLOCK_AB=1 "${bin}" >> "${log}" 2>&1 || status=$?

  if [ "${status}" -ne 0 ]; then
    printf '%s\t%s\tdiagnostic\t\t\t\t\t\t\texit=%s\n' "${side}" "${pair}" "${status}" >> "${SUMMARY}"
    return
  fi

  # `dot_kernel: label=current working_set=<ws> dim=<d> rows=<r> median_ms=<x>
  # ns_per_dot=<y>` 行を全 dim/working_set ぶん抽出する。`sed -nE` で
  # `key=` 直後の値だけを取り出し、キー名同士の数字混入（例: `ns_per_dot` の
  # "50" 誤取得）を避ける（bench_gpu_scaling_ab.sh の教訓を踏襲）。
  while IFS= read -r line; do
    [ -n "${line}" ] || continue
    local ws dim rows median ns
    ws="$(echo "${line}" | sed -nE 's/.*working_set=([a-z_]+) .*/\1/p')"
    dim="$(echo "${line}" | sed -nE 's/.*dim=([0-9]+) .*/\1/p')"
    rows="$(echo "${line}" | sed -nE 's/.*rows=([0-9]+) .*/\1/p')"
    median="$(echo "${line}" | sed -nE 's/.*median_ms=([0-9.]+) .*/\1/p')"
    ns="$(echo "${line}" | sed -nE 's/.*ns_per_dot=([0-9.]+)$/\1/p')"
    printf '%s\t%s\tcurrent\t%s\t%s\t%s\t%s\t%s\t\tok\n' \
      "${side}" "${pair}" "${ws}" "${dim}" "${rows}" "${median}" "${ns}" >> "${SUMMARY}"
  done < <(grep -E '^dot_kernel: label=current ' "${log}" || true)

  # `dot_kernel: block4_ab working_set=<ws> dim=<d> single_row_median_ms=<x>
  # block4_median_ms=<y> ratio=<z>` 行。single/block4 の 2 行へ分解する。
  while IFS= read -r line; do
    [ -n "${line}" ] || continue
    local ws dim single block ratio
    ws="$(echo "${line}" | sed -nE 's/.*working_set=([a-z_]+) .*/\1/p')"
    dim="$(echo "${line}" | sed -nE 's/.*dim=([0-9]+) .*/\1/p')"
    single="$(echo "${line}" | sed -nE 's/.*single_row_median_ms=([0-9.]+) .*/\1/p')"
    block="$(echo "${line}" | sed -nE 's/.*block4_median_ms=([0-9.]+) .*/\1/p')"
    ratio="$(echo "${line}" | sed -nE 's/.*ratio=([0-9.]+)$/\1/p')"
    printf '%s\t%s\tblock_single\t%s\t%s\t\t%s\t\t\tok\n' \
      "${side}" "${pair}" "${ws}" "${dim}" "${single}" >> "${SUMMARY}"
    printf '%s\t%s\tblock4\t%s\t%s\t\t%s\t\t%s\tok\n' \
      "${side}" "${pair}" "${ws}" "${dim}" "${block}" "${ratio}" >> "${SUMMARY}"
  done < <(grep -E '^dot_kernel: block4_ab working_set=' "${log}" || true)

  # `dot_kernel: block4_ab_ref working_set=<ws> dim=<d> ref_median_ms=<x>
  # band=<y>` 行（変更を含まない参照区間の 1 プロセスぶん代表値）。
  while IFS= read -r line; do
    [ -n "${line}" ] || continue
    local ws dim refm band
    ws="$(echo "${line}" | sed -nE 's/.*working_set=([a-z_]+) .*/\1/p')"
    dim="$(echo "${line}" | sed -nE 's/.*dim=([0-9]+) .*/\1/p')"
    refm="$(echo "${line}" | sed -nE 's/.*ref_median_ms=([0-9.]+) .*/\1/p')"
    band="$(echo "${line}" | sed -nE 's/.*band=([0-9.]+)$/\1/p')"
    printf '%s\t%s\tblock_ref\t%s\t%s\t\t%s\t\t%s\tok\n' \
      "${side}" "${pair}" "${ws}" "${dim}" "${refm}" "${band}" >> "${SUMMARY}"
  done < <(grep -E '^dot_kernel: block4_ab_ref working_set=' "${log}" || true)
}

for pair in $(seq 1 "${PAIRS}"); do
  run_one "${BEFORE_BIN}" before "${pair}"
  run_one "${AFTER_BIN}" after "${pair}"
  for i in "${!CAND_LABELS[@]}"; do
    run_one "${BEFORE_BIN}" before "${pair}c${i}"
    run_one "${CAND_PATHS[$i]}" "${CAND_LABELS[$i]}" "${pair}"
  done
done

echo "done: results under ${OUT_DIR}"
