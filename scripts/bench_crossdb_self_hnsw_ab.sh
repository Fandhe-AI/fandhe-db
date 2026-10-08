#!/usr/bin/env bash
# self（wire-server 経由）の `--config exact` と `--config hnsw`（`--search-engine
# hnsw` opt-in。Issue #656・#657・#658）を同一バイナリ・同一 fixture で交互 N
# ペア実行し、`crossdb_bench/run.py` の per-run 結果 JSON を保存する（計測規約
# `docs/design/benchmark-judgement-policy.md` §3〜§5 の「交互 N≥5 ペア・
# per-run 生データ必須・min-of-N＋median 併記・共有環境は参考値」に従う）。
#
# `scripts/bench_scalar_index_crossdb_ab.sh` と異なり、本スクリプトは
# before/after の 2 コミットを比較する前後比較ドライバではない（同一ワーク
# ツリー・同一バイナリのまま `--config` だけを exact/hnsw で切り替える）ため
# `git archive` によるソースツリー複製は行わない。
#
# production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
# 一切変更しない（本スクリプト自体・生成物はテスト・ベンチ専任）。
#
# 使い方:
#   CROSSDB_DIR=<fixture dir> CROSSDB_PYTHON=<python> \
#     scripts/bench_crossdb_self_hnsw_ab.sh
#   AB_PAIRS=<N>（既定 5・5 未満は拒否）
#   CROSSDB_SELF_PORT（既定 15438。並走ジョブとの衝突回避のため既定の 15432
#     以外を使う）
#   CROSSDB_SELF_HNSW_ARGS（hnsw arm にのみ適用される `--hnsw-*` opt-in。
#     `self_hnsw.parse_hnsw_args_env` が許可リスト検証する）
#   OUT_DIR（既定 docs/design/bench-data/crossdb-self-hnsw-ab）
#
# 出力: <OUT_DIR>/<ts>-pair<N>-<arm>/self_<arm>.json
#       <OUT_DIR>/<ts>-env.txt
#
# --summarize <dir> [session_ts] で TSV 集約（フェーズ別 min-of-N・median・
# ratio）。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  DIR="${2:?usage: $0 --summarize <dir> [session_ts]}"
  if [ -n "${3:-}" ]; then
    exec python3 "${REPO_ROOT}/scripts/bench_crossdb_self_hnsw_ab_summarize.py" "${DIR}" "${3}"
  fi
  exec python3 "${REPO_ROOT}/scripts/bench_crossdb_self_hnsw_ab_summarize.py" "${DIR}"
fi

# GITHUB_ACTIONS 下は拒否する（本ベンチは時間依存・spec 閾値を持たない
# 情報提供専用のため CI 非配線・手動実行専用。既存 AB スクリプト群と同方針）。
if [ -n "${GITHUB_ACTIONS:-}" ]; then
  die "this script is manual-only and must not run under GITHUB_ACTIONS"
fi

CROSSDB_DIR="${CROSSDB_DIR:?CROSSDB_DIR (fixture dir containing docs25k.redb/docs25k.jsonl/queries200.jsonl) is required}"
CROSSDB_PYTHON="${CROSSDB_PYTHON:?CROSSDB_PYTHON (venv python with requirements.txt installed) is required}"
AB_PAIRS="${AB_PAIRS:-5}"
if ! [[ "${AB_PAIRS}" =~ ^[0-9]+$ ]] || [ "${AB_PAIRS}" -lt 5 ]; then
  die "AB_PAIRS must be an integer >= 5 (got ${AB_PAIRS})"
fi
export CROSSDB_SELF_PORT="${CROSSDB_SELF_PORT:-15438}"
OUT_DIR="${OUT_DIR:-${REPO_ROOT}/docs/design/bench-data/crossdb-self-hnsw-ab}"
mkdir -p "${OUT_DIR}"

# `self_db.py::SelfServer._default_binary` は `CROSSDB_SELF_BINARY` が
# 継承されていればそちらを既定パスより優先して起動する。ここで固定の既定
# パスだけを存在確認・ハッシュ算出に使うと、呼び出し元が `CROSSDB_SELF_BINARY`
# を設定していた場合に「実際に起動するバイナリ」と「env.txt に記録される
# バイナリ」が食い違い、既定バイナリ未ビルドなら指定済みの実行可能バイナリが
# あっても停止してしまう（codex-review 指摘）。`_default_binary` と同じ優先順位
# でパスを一度だけ解決し、存在確認・ハッシュ算出・`CROSSDB_SELF_BINARY` への
# 再エクスポート（子プロセス起動）のすべてで同じ絶対パスを使う。
WIRE_SERVER_BIN="${CROSSDB_SELF_BINARY:-${REPO_ROOT}/target/release/wire-server}"
case "${WIRE_SERVER_BIN}" in
  /*) : ;;
  *) WIRE_SERVER_BIN="$(pwd)/${WIRE_SERVER_BIN}" ;;
esac
[ -x "${WIRE_SERVER_BIN}" ] || die "wire-server binary not found: ${WIRE_SERVER_BIN} (run: cargo build --release -p fandhe-db-wire-server)"
export CROSSDB_SELF_BINARY="${WIRE_SERVER_BIN}"
PROBE_BIN="${CROSSDB_PLAN_PROBE_BINARY:-${REPO_ROOT}/target/release/examples/crossdb_plan_probe}"
[ -x "${PROBE_BIN}" ] || die "crossdb_plan_probe binary not found: ${PROBE_BIN} (run: cargo build --release -p fandhe-db-engine --example crossdb_plan_probe)"

ROWS_REDB="${CROSSDB_DIR}/docs25k.redb"
QUERIES_FILE="${CROSSDB_DIR}/queries200.jsonl"
[ -f "${ROWS_REDB}" ] || die "fixture not found: ${ROWS_REDB}"
[ -f "${QUERIES_FILE}" ] || die "fixture not found: ${QUERIES_FILE}"

TS="$(date -u +%Y%m%dT%H%M%SZ)"

# `CROSSDB_SELF_HNSW_ARGS` は hnsw arm にのみ意味を持つ opt-in（README・
# self_db.py::run 参照）。この変数を無条件にプロセス環境へ残したまま exact
# arm を起動すると、`self_db.run()` が「exact 構成で --hnsw-* tuning が
# 設定されている」として `ValueError` を送出し `set -e` により全体が停止する
# （hnsw arm 専用の値を exact arm へ漏らさない）。ここで一度取り出したうえで
# 変数自体を unset し、`run_arm` 内で hnsw arm のときだけ再エクスポートする。
HNSW_ARM_ARGS="${CROSSDB_SELF_HNSW_ARGS:-}"
unset CROSSDB_SELF_HNSW_ARGS

# 同時実行プロセスの有無（自プロセス・ps 自体を除く実行中プロセス数の簡易
# スナップショット。`benchmark-judgement-policy.md` §3 が必須とする記録項目。
# `bench_hybrid_latency_ab.sh::record_concurrent_processes` と同型の判定）。
concurrent_processes_snapshot() {
  local self_pid=$$ snapshot
  if ! snapshot="$(ps -eo pid=,stat=,comm= 2>/dev/null)"; then
    echo "unknown (ps unavailable)"
    return 0
  fi
  printf '%s\n' "${snapshot}" \
    | awk -v self="${self_pid}" '$1 != self && $3 != "ps" && $2 ~ /^R/ {n++} END {print n+0}'
}

{
  echo "timestamp_utc: ${TS}"
  echo "wire_server_sha256: $(sha256sum "${WIRE_SERVER_BIN}" | awk '{print $1}')"
  echo "crossdb_plan_probe_sha256: $(sha256sum "${PROBE_BIN}" | awk '{print $1}')"
  echo "git_rev: $(git -C "${REPO_ROOT}" rev-parse HEAD)"
  echo "ab_pairs: ${AB_PAIRS}"
  echo "loadavg_before: $(cat /proc/loadavg 2>/dev/null || echo unavailable)"
  echo "fs_type_crossdb_dir: $(df --output=fstype "${CROSSDB_DIR}" 2>/dev/null | tail -n1 || echo unavailable)"
  echo "hnsw_args: ${HNSW_ARM_ARGS:-(none)}"
  # `benchmark-judgement-policy.md` §3 必須項目（CPU Model name・ISA flags・
  # 同時実行プロセスの有無・`BENCH_DEDICATED_ENV` の設定有無。codex-review P2
  # 指摘・Issue #658）。
  echo "cpu_model: $(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo unavailable)"
  echo "cpu_flags_subset: $(grep -m1 flags /proc/cpuinfo 2>/dev/null | grep -oE '\b(avx2|avx512f|fma|f16c|neon)\b' | tr '\n' ' ' | sed 's/ *$//' || echo unavailable)"
  echo "nproc: $(nproc 2>/dev/null || echo unavailable)"
  echo "concurrent_running_processes_excluding_self: $(concurrent_processes_snapshot)"
  echo "bench_dedicated_env: ${BENCH_DEDICATED_ENV:-<unset>}"
} >"${OUT_DIR}/${TS}-env.txt"

echo "writing per-run results under ${OUT_DIR}/${TS}-pair<N>-<arm>/"

run_arm() {
  local arm="$1" n="$2"
  local out_dir="${OUT_DIR}/${TS}-pair${n}-${arm}"
  mkdir -p "${out_dir}"
  echo "=== pair ${n}: arm=${arm} ==="
  if [ "${arm}" = "hnsw" ] && [ -n "${HNSW_ARM_ARGS}" ]; then
    export CROSSDB_SELF_HNSW_ARGS="${HNSW_ARM_ARGS}"
  else
    unset CROSSDB_SELF_HNSW_ARGS
  fi
  "${CROSSDB_PYTHON}" "${REPO_ROOT}/scripts/crossdb_bench/run.py" \
    --db self --config "${arm}" \
    --rows-file "${ROWS_REDB}" --queries-file "${QUERIES_FILE}" \
    --out-dir "${out_dir}" --workdir "${out_dir}"
}

for n in $(seq 1 "${AB_PAIRS}"); do
  run_arm exact "${n}"
  run_arm hnsw "${n}"
done

echo "done: ${AB_PAIRS} pairs written under ${OUT_DIR}/${TS}-pair*-{exact,hnsw}/"
echo "summarize with: $0 --summarize ${OUT_DIR} ${TS}"
