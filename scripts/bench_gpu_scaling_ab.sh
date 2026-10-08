#!/usr/bin/env bash
# `crates/engine/benches/gpu_scaling_bench.rs`（Issue #178 ポインタ・手動専用の
# 情報提供ベンチ）を before/after 2 バイナリで交互 min-of-N 実行するための薄い
# ドライバ。Issue #533（親 #531・#460・#455）が要求する「#532（クエリタイル化）
# の前後比較」を `docs/design/benchmark-judgement-policy.md` の交互実行・
# per-run 生データ保持・参照区間ノイズ帯規約に沿って計測するために追加した。
#
# 呼び出し元は人間の運用者（`make` 経由のターゲットは設けない。ベンチそのものが
# 手動専用・CI 非配線のため）。呼び出し先は `BEFORE_BIN`/`AFTER_BIN` として渡す
# 2 つの `gpu_scaling_bench` 実行ファイル（`cargo bench --bench gpu_scaling_bench
# -p fandhe-db-engine --no-run --message-format=json` で得た成果物を退避したもの）。
# production コード（`crates/engine/src/`）・既存ベンチハーネスは一切変更しない。
#
# 使い方:
#   BEFORE_BIN=<path> AFTER_BIN=<path> scripts/bench_gpu_scaling_ab.sh \
#     [PAIRS] [POINTS...]
#   PAIRS: 交互実行するペア数（既定 5・正整数のみ）。
#   POINTS: "rows:dim:batch" 形式の規模点（十進数字のみ）。省略時は既定 1 点
#     20000:128:8 のみ（スモーク用）。複数指定可。
#   OUT_DIR: 出力先（既定 _/bench/gpu-scaling-ab/<UTC timestamp>）。
#
# 出力: <OUT_DIR>/<rows>-<dim>-<batch>/<pair>-<before|after>.log に stdout 全文、
# <OUT_DIR>/summary.tsv に集計用の 1 行 1 run の TSV を追記する。
# 各 run の直前に /proc/loadavg・nvidia-smi のクロック/温度を同じログへ書き、
# 計測環境のノイズ源を再現性のため残す。
#
# 記録の保持（codex-review P1 指摘・PR #580）: OUT_DIR の既定値は本リポの
# `.gitignore` 対象（`_/`）配下のため、実行後 summary.tsv を残す場合は
# `docs/design/bench-data/gpu-scaling-ab/<UTC timestamp>-summary.tsv` へ
# 明示的にコピーし、対応する docs/design/*.md の実測表からそのパスを参照する
# こと（benchmark-judgement-policy.md §3 の per-run 生データ保持契約）。
# 生ログ全文（*.log）まで tracked にする必要はない。
#
# 実行順序（規約: 逐次実行にしない・生ログを残す・skip/unavailable を握りつぶさない）:
#   各規模点について pair=1..PAIRS の順で before → after を実行する。
#
# i8 パック常駐経路（Issue #543・親 #541）: `I8_OVERSAMPLE` が設定されている
# 場合のみ `BENCH_GPU_SCALING_I8_OVERSAMPLE` として両バイナリへパススルーする
# （before バイナリは未知の env を読まないため無害。未設定時は harness 側の
# `DEFAULT_I8_OVERSAMPLE` が使われるだけで i8 計測自体は新 after バイナリでは
# 常に試みられる。i8 経路は `e14d53f` より前には存在せず、before 側の
# `gpu_scaling_i8:`/`gpu_scaling_i8: not measurable` 行は常に「出ない」ことが
# 期待値——これ自体が「i8 の before は作れない」という Issue #543 の設計判断の
# 直接的な現れ）。summary.tsv の末尾へ `i8_oversample`/`i8_p50`/`i8_p95`/
# `i8_recall`/`i8_mismatch`/`i8_status`/`i8_reason` 列を追加する（既存 10 列の
# 並び・意味は不変。before 側・i8 未計測行は数値列を空欄のまま維持しつつ、
# `i8_status`（`unsupported`＝ログに i8 出力が無く、かつプロセスが正常終了
# 〔典型的には before 側で i8 経路自体が存在しない〕、または `not measurable`
# 行でバイナリが自己申告した場合／`measured`＝i8 計測成功／`failed`＝i8 出力が
# 無くプロセスが異常終了した場合）と `i8_reason`（`unsupported`〔`not
# measurable` 行由来〕・`failed` 時に `gpu_scaling_i8: not measurable ...
# reason="..."` から抽出。無ければ `unknown`）で、A/B/C（`line` 列）が
# `measured` でも i8 だけが失敗した run を「未対応」と区別する。分類は
# `I8_OVERSAMPLE` の設定有無ではなく、ログの実出力（`gpu_scaling_i8:` 行の
# 有無）とプロセスの終了コードのみを根拠にする（codex-review P2・Cursor
# Bugbot 重複指摘・PR #605）。
#
# f16 算術版 S0 シェーダの前後比較（Issue #540。親 #402・#539）:
# `QUERY_F16_EXACT=1` を設定すると両バイナリへ `BENCH_GPU_SCALING_QUERY_F16_EXACT`
# としてパススルーする。**注意（codex-review P2 指摘・PR #611）**: これは
# プロセス起動を落とさないという意味では「無害」だが、before バイナリが
# 本 env の解釈コード（`harness/gpu_scaling.rs::parse_query_f16_exact`）を
# 持たない場合は単に無視されるだけで、before はクエリを丸めない・after は
# クエリを f16 厳密往復可能な値へ丸める、という**異なるクエリ集合の比較**に
# なってしまい、`docs/design/gpu-batch-f16-arith.md` §8.1〜8.3 が解消した
# はずの交絡（クエリの違いとシェーダの違いが同時に変動する）が本スクリプト
# 経由の比較では再発する。before/after 双方が `parse_query_f16_exact` を
# 持つ場合（= 両バイナリとも Issue #540 の丸め対応を含む場合）に限り本
# opt-in は交絡なしで使える。それ以外（本 Issue の変更前後を跨ぐ比較等）
# では本スクリプトではなく、同一プロセス内で `Unpack`／`F16Arith` を強制
# する `BENCH_GPU_SCALING_SHADER_AB=1`（`bench-internals` feature・単一
# after バイナリのみで完結する in-process A/B。`gpu_scaling_bench.rs::
# measure_shader_ab`・`docs/design/gpu-batch-f16-arith.md` §8.3.1 参照）を
# 使うこと。summary.tsv 末尾へ `f16_arith_dispatches`/
# `f16_arith_guard_fallbacks` 列を追加する（`gpu_scaling_stats:` 行から
# 抽出。既存 18 列の並び・意味は不変。未設定時は既定挙動〔クエリ丸めなし〕
# のまま不変）。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

: "${BEFORE_BIN:?BEFORE_BIN must be set to the before gpu_scaling_bench binary path}"
: "${AFTER_BIN:?AFTER_BIN must be set to the after gpu_scaling_bench binary path}"

[[ -x "${BEFORE_BIN}" ]] || die "BEFORE_BIN is not an executable file: ${BEFORE_BIN}"
[[ -x "${AFTER_BIN}" ]] || die "AFTER_BIN is not an executable file: ${AFTER_BIN}"

PAIRS="${1:-5}"
if ! [[ "${PAIRS}" =~ ^[0-9]+$ ]] || [ "${PAIRS}" -lt 1 ]; then
  die "PAIRS must be a positive integer, got: ${PAIRS}"
fi
shift || true

POINTS=("$@")
if [ "${#POINTS[@]}" -eq 0 ]; then
  POINTS=("20000:128:8")
fi

for point in "${POINTS[@]}"; do
  if ! [[ "${point}" =~ ^[0-9]+:[0-9]+:[0-9]+$ ]]; then
    die "point must match rows:dim:batch (digits only), got: ${point}"
  fi
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_OUT_DIR="${REPO_ROOT}/_/bench/gpu-scaling-ab/$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="${OUT_DIR:-${DEFAULT_OUT_DIR}}"
mkdir -p "${OUT_DIR}"

if [ -n "${I8_OVERSAMPLE:-}" ] && ! [[ "${I8_OVERSAMPLE}" =~ ^[0-9]+$ ]]; then
  die "I8_OVERSAMPLE must be a positive integer when set, got: ${I8_OVERSAMPLE}"
fi

# Issue #540: `harness/gpu_scaling.rs::parse_query_f16_exact` が受理する値
# （未設定 or "1"）と同じ許容集合をシェル側でも fail-closed に検査してから
# 子プロセスへ渡す（coding-rust.md「untrusted 入力の扱い」: env インジェクション
# 防止のため許可値のみ受理する）。
if [ -n "${QUERY_F16_EXACT:-}" ] && [ "${QUERY_F16_EXACT}" != "1" ]; then
  die "QUERY_F16_EXACT must be unset or \"1\" when set, got: ${QUERY_F16_EXACT}"
fi

SUMMARY="${OUT_DIR}/summary.tsv"
if [ ! -f "${SUMMARY}" ]; then
  # i8_status/i8_reason（codex-review P2・Cursor Bugbot 重複指摘・PR #605）:
  # 既存 `line` 列（A/B/C 経路が測定できたかの status）だけでは、A/B/C が
  # 測定できて `line=measured` になった run で i8 経路だけが失敗した場合と、
  # そもそも `I8_OVERSAMPLE` 未指定で i8 を計測対象外とした場合（典型的には
  # before 側）を summary.tsv 上で区別できない。i8 専用の status
  # （`unsupported`／`measured`／`failed`）と、`failed` 時の理由
  # （`gpu_scaling_i8: not measurable ... reason="..."` から抽出）を
  # 独立の列として追加する。
  # f16_arith_dispatches/f16_arith_guard_fallbacks（Issue #540。
  # `gpu_scaling_stats:` 行〔`gpu_scaling:` 結果行とは接頭辞が異なる別行〕から
  # 抽出。行そのものが無い run（旧 before バイナリ等）は空欄のまま。
  printf 'point\tside\tpair\tcpu_p50\tcpu_p95\tf16_p50\tf16_p95\tf32_p50\tf32_p95\tmismatch\tline\ti8_oversample\ti8_p50\ti8_p95\ti8_recall\ti8_mismatch\ti8_status\ti8_reason\tf16_arith_dispatches\tf16_arith_guard_fallbacks\n' > "${SUMMARY}"
fi

# 1 行の `gpu_scaling: ...` 出力（正常計測行のみ）から TSV フィールドを
# 取り出す。skip/not measurable/gpu unavailable 行は集計対象外のまま
# ログにのみ残し、summary.tsv には別途 status 行として記録する。
extract_field() {
  local line="$1" key="$2"
  # 例: "cpu_simd_p50=1234us" -> 1234
  # 注意: `grep -oE '[0-9]+'` を素朴に重ねると "cpu_simd_p50" 自体に含まれる
  # "50" まで数値として拾ってしまう（例: p50/p95 系のキー名と衝突する）。
  # 必ず `key=` の直後の数値だけを sed で取り出す。
  local matched
  matched="$(echo "${line}" | grep -oE "${key}=[0-9]+us" | head -1 || true)"
  [ -n "${matched}" ] || { echo ""; return; }
  echo "${matched}" | sed -E "s/^${key}=([0-9]+)us\$/\\1/"
}

run_one() {
  local bin="$1" side="$2" pair="$3" rows="$4" dim="$5" batch="$6" point="$7"
  local point_dir="${OUT_DIR}/${point//:/-}"
  mkdir -p "${point_dir}"
  local log="${point_dir}/${pair}-${side}.log"

  # 既存の生ログを黙って上書きしない（docs/design/benchmark-judgement-policy.md
  # §3 の per-run 生データ保持契約）。同じ OUT_DIR・規模点で再実行すると
  # summary.tsv には追記される一方 log ファイルは上書きされ、過去の集計行に
  # 対応する環境情報・出力全文が失われる不整合を防ぐため、既存ログがあれば
  # 実行前に拒否する（codex-review P1 指摘。PR #580）。
  if [ -e "${log}" ]; then
    die "raw log already exists (refusing to overwrite): ${log}. Use a fresh OUT_DIR for a new run."
  fi

  {
    echo "# pre-run environment snapshot"
    echo "loadavg: $(cat /proc/loadavg 2>/dev/null || echo unavailable)"
    if command -v nvidia-smi >/dev/null 2>&1; then
      echo "nvidia-smi clocks.sm,temperature.gpu:"
      nvidia-smi --query-gpu=clocks.sm,temperature.gpu --format=csv,noheader 2>/dev/null || echo unavailable
    else
      echo "nvidia-smi: not found"
    fi
    echo "# gpu_scaling_bench output"
  } > "${log}"

  local status=0
  # Issue #540: `I8_OVERSAMPLE` と同じ「設定時のみパススルー」方式。事前検証
  # （`QUERY_F16_EXACT` fail-closed 検査）を経た値のみここへ渡す。
  local -a env_args=(
    "BENCH_GPU_SCALING_ROWS=${rows}"
    "BENCH_GPU_SCALING_DIMS=${dim}"
    "BENCH_GPU_SCALING_BATCH=${batch}"
  )
  if [ -n "${I8_OVERSAMPLE:-}" ]; then
    env_args+=("BENCH_GPU_SCALING_I8_OVERSAMPLE=${I8_OVERSAMPLE}")
  fi
  if [ -n "${QUERY_F16_EXACT:-}" ]; then
    env_args+=("BENCH_GPU_SCALING_QUERY_F16_EXACT=${QUERY_F16_EXACT}")
  fi
  env "${env_args[@]}" "${bin}" >> "${log}" 2>&1 || status=$?

  local result_line
  result_line="$(grep -E '^gpu_scaling: rows=' "${log}" | tail -1 || true)"

  # f16 算術版 S0 シェーダの dispatch カウンタ（Issue #540）。
  # `gpu_scaling_stats:` は既存 `gpu_scaling:`/`gpu_scaling_i8:` いずれとも
  # 接頭辞が異なるため誤って `result_line`/`i8_line` へ混入しない。
  local stats_line f16_arith_dispatches f16_arith_guard_fallbacks
  stats_line="$(grep -E '^gpu_scaling_stats: rows=' "${log}" | tail -1 || true)"
  if [ -n "${stats_line}" ]; then
    f16_arith_dispatches="$(echo "${stats_line}" | sed -nE 's/.* f16_arith_dispatches=([0-9]+) .*/\1/p')"
    f16_arith_guard_fallbacks="$(echo "${stats_line}" | sed -nE 's/.* f16_arith_guard_fallbacks=([0-9]+) .*/\1/p')"
  else
    f16_arith_dispatches=""
    f16_arith_guard_fallbacks=""
  fi

  # i8 経路（Issue #543）の結果行。`gpu_scaling_i8:` は既存 `gpu_scaling:` の
  # grep（`^gpu_scaling: rows=`）とは接頭辞が異なるため誤って上の
  # `result_line` へ混入しない（`harness/gpu_scaling.rs` ドキュメンテーション
  # コメント・回帰テスト `gpu_scaling_i8_result_line_has_expected_prefix_and_fields`
  # 参照）。
  local i8_line
  i8_line="$(grep -E '^gpu_scaling_i8: rows=' "${log}" | tail -1 || true)"
  local i8_oversample i8_p50 i8_p95 i8_recall i8_mismatch i8_status i8_reason
  if [ -n "${i8_line}" ]; then
    # 注意: `grep -oE '[0-9]+'` を素朴に重ねる二段抽出は使わない——
    # `i8_mismatch`/`i8_recall_at_k` というキー名自体が数字 "8" を含むため、
    # `extract_field`（既存の cpu/f16/f32 列と同じ、`key=` 直後の数値のみを
    # sed で取り出す方式）と同型の単発 sed 抽出に統一する
    # （`extract_field` は接尾辞 `us` 前提のため、`us` を伴わない
    # `oversample`/`i8_mismatch`/`i8_recall_at_k` はここで個別に抽出する）。
    i8_oversample="$(echo "${i8_line}" | sed -nE 's/.* oversample=([0-9]+) .*/\1/p')"
    i8_p50="$(extract_field "${i8_line}" gpu_i8_p50)"
    i8_p95="$(extract_field "${i8_line}" gpu_i8_p95)"
    i8_recall="$(echo "${i8_line}" | sed -nE 's/.*i8_recall_at_k=([0-9.]+)$/\1/p')"
    i8_mismatch="$(echo "${i8_line}" | sed -nE 's/.* i8_mismatch=([0-9]+) .*/\1/p')"
    i8_status="measured"
    i8_reason=""
  else
    # `gpu_scaling_i8: rows=`（成功行）が出なかった場合。分類は
    # `I8_OVERSAMPLE`（未指定時は harness 側が `DEFAULT_I8_OVERSAMPLE` を
    # 使うだけで i8 計測自体は常に試みられる。`gpu_scaling_bench.rs`
    # 参照）の設定有無に依存させない——ドキュメント記載の実行方法
    # （`I8_OVERSAMPLE=4` 指定）だと i8 未対応の旧 before バイナリの
    # 全行が `failed` 誤判定になり、逆に環境変数を未設定にすると新
    # after バイナリの実際の `not measurable` 出力が `unsupported`
    # 誤判定になっていた（codex-review P2・Cursor Bugbot 重複指摘・
    # PR #605）。ログの実出力のみを根拠に判定する:
    #   1. `harness/gpu_scaling.rs::format_i8_unavailable_line` の
    #      `gpu_scaling_i8: not measurable ... reason="..."` 行が
    #      あれば、バイナリが i8 経路を自己申告で不可としたケース
    #      （GPU 側の実行時条件等）として `unsupported` に分類し
    #      reason を抽出する。
    #   2. その行も無く、かつ計測プロセス自体が非 0 終了していた
    #      場合は、i8 経路が出力前に異常終了した可能性が高いため
    #      `failed` に分類する（`status_line` 同様 log 全文は残る
    #      ため詳細はそちらで追える）。
    #   3. いずれの行も無く、かつプロセスが正常終了していた場合は
    #      バイナリ自体に i8 経路が存在しない（典型的には before 側）
    #      とみなし `unsupported` に分類する。
    i8_oversample=""
    i8_p50=""
    i8_p95=""
    i8_recall=""
    i8_mismatch=""
    local i8_not_measurable_line
    i8_not_measurable_line="$(grep -E '^gpu_scaling_i8: not measurable' "${log}" | tail -1 || true)"
    if [ -n "${i8_not_measurable_line}" ]; then
      i8_status="unsupported"
      i8_reason="$(echo "${i8_not_measurable_line}" | sed -nE 's/.*reason="([^"]*)".*/\1/p')"
      [ -n "${i8_reason}" ] || i8_reason="unknown"
    elif [ "${status}" -ne 0 ]; then
      i8_status="failed"
      i8_reason="unknown"
    else
      i8_status="unsupported"
      i8_reason=""
    fi
  fi

  if [ -n "${result_line}" ]; then
    local cpu_p50 cpu_p95 f16_p50 f16_p95 f32_p50 f32_p95 mismatch
    cpu_p50="$(extract_field "${result_line}" cpu_simd_p50)"
    cpu_p95="$(extract_field "${result_line}" cpu_simd_p95)"
    f16_p50="$(extract_field "${result_line}" gpu_f16_p50)"
    f16_p95="$(extract_field "${result_line}" gpu_f16_p95)"
    f32_p50="$(extract_field "${result_line}" gpu_f32_p50)"
    f32_p95="$(extract_field "${result_line}" gpu_f32_p95)"
    mismatch="$(echo "${result_line}" | grep -oE 'mismatch=[0-9]+' | grep -oE '[0-9]+' || true)"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "${point}" "${side}" "${pair}" "${cpu_p50}" "${cpu_p95}" "${f16_p50}" "${f16_p95}" \
      "${f32_p50}" "${f32_p95}" "${mismatch}" "measured" \
      "${i8_oversample}" "${i8_p50}" "${i8_p95}" "${i8_recall}" "${i8_mismatch}" \
      "${i8_status}" "${i8_reason}" \
      "${f16_arith_dispatches}" "${f16_arith_guard_fallbacks}" >> "${SUMMARY}"
  else
    local status_line
    status_line="$(grep -E '^gpu_scaling: (skip|not measurable|gpu unavailable)' "${log}" | tail -1 || echo "exit=${status}")"
    printf '%s\t%s\t%s\t\t\t\t\t\t\t\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "${point}" "${side}" "${pair}" "${status_line//$'\t'/ }" \
      "${i8_oversample}" "${i8_p50}" "${i8_p95}" "${i8_recall}" "${i8_mismatch}" \
      "${i8_status}" "${i8_reason}" \
      "${f16_arith_dispatches}" "${f16_arith_guard_fallbacks}" >> "${SUMMARY}"
  fi
}

for point in "${POINTS[@]}"; do
  IFS=':' read -r rows dim batch <<< "${point}"
  for pair in $(seq 1 "${PAIRS}"); do
    run_one "${BEFORE_BIN}" before "${pair}" "${rows}" "${dim}" "${batch}" "${point}"
    run_one "${AFTER_BIN}" after "${pair}" "${rows}" "${dim}" "${batch}" "${point}"
  done
done

echo "done: results under ${OUT_DIR}"
