#!/usr/bin/env bash
# crossdb 横断ベンチ（`scripts/crossdb_bench/run_all.sh`）を N ラウンド
# 繰り返し実行し、self・対照 DB（Elasticsearch を含む全 DB）双方の
# per-run 生データを `docs/design/benchmark-judgement-policy.md` の規約
# （交互 N≥5・per-run 生データ必須）に沿って分離保存する（Issue #848）。
#
# `scripts/bench_crossdb_self_hnsw_ab.sh` 等の既存 AB ドライバは before/after
# または exact/hnsw の 2 arm を厳密に輪番させる設計だが、本スクリプトは
# `run_all.sh` が 1 回で走らせる十数 arm（self exact/hnsw/nosql・対照 DB
# 各 exact/hnsw 構成）をまとめて 1 ラウンドとみなし、そのラウンドを
# `AB_PAIRS`（既定 5）回繰り返すラウンドロビン方式を取る（cand が 1 個では
# なく DB の数だけあるため、baseline→cand→baseline→cand... という厳密な
# 2 arm 輪番を適用できない構造的な理由による意図的な逸脱。この逸脱の
# 評価上の扱いは `docs/design/benchmark-judgement-policy.md` §3「多 arm
# 横断ベンチの輪番（意図的な例外）」に明示済み）。
#
# production コード（`crates/engine/src/`・`crates/wire-server/src/`）は
# 一切変更しない（本スクリプト・生成物はテスト・ベンチ専任）。
#
# 使い方:
#   CROSSDB_DIR=<fixture dir> CROSSDB_PYTHON=<venv python> \
#     scripts/bench_crossdb_ab.sh
#   AB_PAIRS=<N>（既定 5・5 未満は拒否）
#   BENCH_DEDICATED_ENV=1（専有環境で実施した場合にのみ設定。env.txt へ記録
#     するだけで動作は変えない）
#
# 出力: <CROSSDB_DIR>/results/<ts>-round<N>/*.json・
#       <CROSSDB_DIR>/logs/<ts>-round<N>/*.log
#       （ラウンドディレクトリ名にセッション起動時刻 `<ts>` を含めるのは、
#       同じ `CROSSDB_DIR` へ複数セッションを実行した際に前回の JSON が
#       `results/round<N>` に残留して混在するのを防ぐため）
#       docs/design/bench-data/crossdb-<ts>-ab/env.txt
#
# --summarize <dir> <rounds> [<round_dir_prefix>] で Markdown 表を出力する
# （scripts/bench_crossdb_ab_summarize.py の実体へ委譲。`<round_dir_prefix>`
# 省略時は既定 `round`（旧セッション形式・`docs/design/bench-data/
# crossdb-20260918T142251Z-ab/` 等の既存コミット済みデータとの後方互換）。
# 本スクリプトが最後に出力する `summarize with: ...` コマンドはセッション
# 固有の prefix を渡す）。

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "${1:-}" = "--summarize" ]; then
  shift
  exec python3 "${REPO_ROOT}/scripts/bench_crossdb_ab_summarize.py" "$@"
fi

# GITHUB_ACTIONS 下は拒否する（本ベンチは時間依存・spec 閾値を持たない
# 情報提供専用のため CI 非配線・手動実行専用。既存 AB スクリプト群と同方針）。
if [ -n "${GITHUB_ACTIONS:-}" ]; then
  die "this script is manual-only and must not run under GITHUB_ACTIONS"
fi

CROSSDB_DIR="${CROSSDB_DIR:?CROSSDB_DIR (fixture dir containing docs25k.redb/docs25k.jsonl/queries200.jsonl) is required}"
CROSSDB_PYTHON="${CROSSDB_PYTHON:?CROSSDB_PYTHON (venv python with requirements.txt installed) is required}"
export CROSSDB_DIR CROSSDB_PYTHON
AB_PAIRS="${AB_PAIRS:-5}"
if ! [[ "${AB_PAIRS}" =~ ^[0-9]+$ ]] || [ "${AB_PAIRS}" -lt 5 ]; then
  die "AB_PAIRS must be an integer >= 5 (got ${AB_PAIRS})"
fi

# `self_db.py::SelfServer._default_binary` は `CROSSDB_SELF_BINARY` が
# 相対パスならそのプロセスの cwd を基準に再解決するため、ここで絶対パス化した
# 値を export せずに使うと、以下で sha256 を取るバイナリと `run_all.sh` の
# 子プロセスが実際に起動するバイナリが異なる cwd 解決により食い違いうる
# （`scripts/bench_crossdb_self_hnsw_ab.sh` と同型の対策）。ここで一度だけ
# パスを解決し、存在確認・ハッシュ算出・`CROSSDB_SELF_BINARY` への
# 再代入のいずれにも同じ絶対パスを使う。
WIRE_SERVER_BIN="${CROSSDB_SELF_BINARY:-${REPO_ROOT}/target/release/wire-server}"
case "${WIRE_SERVER_BIN}" in
  /*) : ;;
  *) WIRE_SERVER_BIN="$(pwd)/${WIRE_SERVER_BIN}" ;;
esac
[ -x "${WIRE_SERVER_BIN}" ] || die "wire-server binary not found: ${WIRE_SERVER_BIN} (run: cargo build --release -p fandhe-db-wire-server)"
export CROSSDB_SELF_BINARY="${WIRE_SERVER_BIN}"

TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DOC_DIR="${REPO_ROOT}/docs/design/bench-data/crossdb-${TS}-ab"
mkdir -p "${OUT_DOC_DIR}"

# CPU モデル・コア数・loadavg は macOS（`sysctl`）／Linux（`/proc/*`）の両対応
# で取得する（本 Issue の計測環境が macOS のため。既存 AB ドライバ群は
# Linux 専有の `/proc/loadavg` 等を前提にしており macOS では unavailable に
# なる）。判定は `uname -s` で行う（`command -v sysctl` は procps 由来の
# `sysctl` バイナリを $PATH に持つ Linux 環境でも真になり、`machdep.cpu.*`／
# `vm.loadavg` のような BSD 専用キーの誤参照や `fs_type_of` の macOS 専用
# `mount` 出力解析への誤分岐を招く）。
IS_MACOS=0
[ "$(uname -s 2>/dev/null)" = "Darwin" ] && IS_MACOS=1
cpu_model() {
  if [ "${IS_MACOS}" = 1 ] && sysctl -n machdep.cpu.brand_string 2>/dev/null; then
    return 0
  fi
  grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo unavailable
}
cpu_count() {
  if [ "${IS_MACOS}" = 1 ]; then
    sysctl -n hw.ncpu 2>/dev/null || echo unavailable
    return 0
  fi
  nproc 2>/dev/null || echo unavailable
}
loadavg_now() {
  if [ "${IS_MACOS}" = 1 ] && sysctl -n vm.loadavg 2>/dev/null; then
    return 0
  fi
  cat /proc/loadavg 2>/dev/null || echo unavailable
}
concurrent_processes_snapshot() {
  local self_pid=$$ snapshot
  if ! snapshot="$(ps -eo pid=,stat=,comm= 2>/dev/null)"; then
    echo "unknown (ps unavailable)"
    return 0
  fi
  printf '%s\n' "${snapshot}" \
    | awk -v self="${self_pid}" '$1 != self && $3 != "ps" && $2 ~ /^R/ {n++} END {print n+0}'
}
fs_type_of() {
  # macOS の `df` には `-T`（ファイルシステム種別列）が無い。`df` が解決した
  # デバイス（シンボリックリンク・bind mount を辿った後の実マウント点）を
  # 手掛かりに `mount` 出力（`<device> on <path> (<fstype>, ...)`）から拾う。
  # Linux では `df -T` の 2 列目（filesystem type）をそのまま使う。
  if [ "${IS_MACOS}" = 1 ]; then
    local dev
    dev="$(df "$1" 2>/dev/null | tail -n1 | awk '{print $1}')"
    [ -n "${dev}" ] || { echo unavailable; return 0; }
    mount 2>/dev/null | awk -v d="${dev}" '$1 == d {print; exit}' \
      | sed -n 's/.*(\([a-zA-Z0-9_]*\),.*/\1/p'
    return 0
  fi
  df -T "$1" 2>/dev/null | tail -n1 | awk '{print $2}' || echo unavailable
}
docker_image_digest() {
  # $1 = image reference（例 pgvector/pgvector:pg17）。`latest`／メジャータグは
  # 計測ごとに digest を記録する既定方針（README 参照）。
  docker image inspect --format '{{index .RepoDigests 0}}' "$1" 2>/dev/null || echo "unavailable($1)"
}

{
  echo "# crossdb 再計測（Issue #848）環境記録"
  echo "timestamp_utc: ${TS}"
  echo "wire_server_sha256: $(shasum -a 256 "${WIRE_SERVER_BIN}" 2>/dev/null | awk '{print $1}')"
  echo "git_rev: $(git -C "${REPO_ROOT}" rev-parse HEAD)"
  echo "ab_pairs: ${AB_PAIRS}"
  echo "cpu_model: $(cpu_model)"
  echo "nproc: $(cpu_count)"
  echo "loadavg_before: $(loadavg_now)"
  echo "concurrent_running_processes_excluding_self: $(concurrent_processes_snapshot)"
  echo "bench_dedicated_env: ${BENCH_DEDICATED_ENV:-<unset>}"
  echo "docker_version: $(docker version --format '{{.Server.Version}}' 2>/dev/null || echo unavailable)"
  for img in pgvector/pgvector:pg17 qdrant/qdrant:latest mysql:9 \
    mongodb/mongodb-atlas-local:latest mongo:8 redis:8 \
    docker.elastic.co/elasticsearch/elasticsearch:9.1.4; do
    echo "docker_image[${img}]: $(docker_image_digest "${img}")"
  done
  echo "fs_type_crossdb_dir: $(fs_type_of "${CROSSDB_DIR}")"
} >"${OUT_DOC_DIR}/env.txt"

# `results/round<N>` を毎回同じ名前で再利用すると、同じ `CROSSDB_DIR` に対して
# 本スクリプトを複数回実行した際に前回セッションの JSON が残留・混在し、
# summarize が古い round の生データを新しいセッションの計測と誤って
# 混ぜて集計しうる。`TS`（本セッションの起動時刻。数字・`T`・`Z` のみで
# `CROSSDB_RUN_TAG` の許容文字集合を満たす）を prefix に含めたセッション
# 固有のラウンドディレクトリ名にすることで、セッションをまたいだ残留を防ぐ。
# `run_all.sh` は `CROSSDB_DIM` 指定時に結果・ログの出力先を
# `results/d${CROSSDB_DIM}/<RUN_TAG>`／`logs/d${CROSSDB_DIM}/<RUN_TAG>` へ
# ネストする（`scripts/crossdb_bench/run_all.sh` 参照）。この `d${CROSSDB_DIM}/`
# は `CROSSDB_RUN_TAG`（`[A-Za-z0-9_-]` のみ許容。`/` を含めると run_all.sh が
# 拒否する）には含められないため、`RUN_TAG_PREFIX`（`CROSSDB_RUN_TAG` へ渡す
# 値）と `SUMMARIZE_PREFIX`（表示・`--summarize` 呼び出しに使う値。
# `summarize.py` は `os.path.join(dir, "results", f"{prefix}{r}")` で
# 結合するだけなので `/` を含んでいてよい）を分けて持つ。両者を分けずに
# `${TS}-round` のままにすると、本スクリプトが表示するパス・
# `--summarize` 呼び出しコマンドが実際の出力先と食い違い、そのままコピー
# した summarize コマンドが `results/${TS}-roundN/` を探しに行って何も
# 見つからない（または既定 dim の残留データを誤って拾う）。`run_all.sh`
# と同じ検証（十進数字のみ）を通したうえで `SUMMARIZE_PREFIX` にだけ
# `d${CROSSDB_DIM}/` を含める。
DIM="${CROSSDB_DIM:-}"
case "${DIM}" in
  '') DIM_PREFIX='' ;;
  *[!0-9]*)
    die "CROSSDB_DIM must contain only decimal digits (got: ${DIM})"
    ;;
  *) DIM_PREFIX="d${DIM}/" ;;
esac
RUN_TAG_PREFIX="${TS}-round"
SUMMARIZE_PREFIX="${DIM_PREFIX}${RUN_TAG_PREFIX}"

echo "writing per-round results under ${CROSSDB_DIR}/results/${SUMMARIZE_PREFIX}<N>/ (logs: ${CROSSDB_DIR}/logs/${SUMMARIZE_PREFIX}<N>/)"

FAILED_ROUNDS=()
for n in $(seq 1 "${AB_PAIRS}"); do
  echo "=== round ${n}/${AB_PAIRS} $(date -u +%H:%M:%SZ) loadavg=$(loadavg_now) ==="
  {
    echo "round_${n}_loadavg_start: $(loadavg_now)"
  } >>"${OUT_DOC_DIR}/env.txt"
  if ! CROSSDB_RUN_TAG="${RUN_TAG_PREFIX}${n}" bash "${REPO_ROOT}/scripts/crossdb_bench/run_all.sh"; then
    echo "round ${n}: run_all.sh reported failures (see logs/${SUMMARIZE_PREFIX}${n}/*.log for FAILED entries)" >&2
    FAILED_ROUNDS+=("${n}")
  fi
  {
    echo "round_${n}_loadavg_end: $(loadavg_now)"
  } >>"${OUT_DOC_DIR}/env.txt"
done

if [ "${#FAILED_ROUNDS[@]}" -gt 0 ]; then
  echo "ERROR: rounds with at least one FAILED arm: ${FAILED_ROUNDS[*]} (see logs/${SUMMARIZE_PREFIX}<N>/*.log). generated JSON for successful arms is still usable; do not silently drop missing arms." >&2
  echo "env record: ${OUT_DOC_DIR}/env.txt" >&2
  echo "summarize with: $0 --summarize ${CROSSDB_DIR} ${AB_PAIRS} ${SUMMARIZE_PREFIX}" >&2
  exit 1
fi

echo "done: ${AB_PAIRS} rounds written under ${CROSSDB_DIR}/results/${SUMMARIZE_PREFIX}*/"
echo "env record: ${OUT_DOC_DIR}/env.txt"
echo "summarize with: $0 --summarize ${CROSSDB_DIR} ${AB_PAIRS} ${SUMMARIZE_PREFIX}"
