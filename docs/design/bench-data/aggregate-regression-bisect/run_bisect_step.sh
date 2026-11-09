#!/usr/bin/env bash
# Issue #1491: 1 コミット分の self exact 計測ドライバ（bisect・A/B・ref で共通に使う）。
# 使い方: BENCH_TARGET=<共有 CARGO_TARGET_DIR> run_bisect_step.sh <sha> <out_dir> [port]
#   - <sha> を git archive で一時領域へ展開し、wire-server を release ビルドする
#   - redb の形式・カタログはコミットごとに異なり別コミットの fixture は開けない／取り違えると
#     誤計測になるため、そのコミット自身の seed_docs で fixture（25,000 行・128 次元）を作り直す
#   - 計測ハーネスもそのコミット自身の scripts/crossdb_bench/run.py（新しい phase を
#     古いバイナリへ流して計測全体が止まるのを避けるため）
#   - 出力: <out_dir>/self_exact.json と stdout.log。一時領域は終了時に削除する
set -euo pipefail

SHA="${1:?usage: run_bisect_step.sh <sha> <out_dir> [port]}"
OUT="${2:?out_dir required}"
PORT="${3:-15491}"
[[ "${SHA}" =~ ^[0-9a-f]{7,40}$ ]] || { echo "invalid sha: ${SHA}" >&2; exit 2; }
[[ "${PORT}" =~ ^[0-9]+$ ]] || { echo "invalid port: ${PORT}" >&2; exit 2; }
: "${BENCH_TARGET:?BENCH_TARGET (shared CARGO_TARGET_DIR) must be set}"
PY="${CROSSDB_PYTHON:-$(command -v python3)}"
REPO_ROOT="$(git rev-parse --show-toplevel)"

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT
SRC="${TMP}/src"; FX="${TMP}/fx"; RES="${TMP}/res"
mkdir -p "${OUT}"; OUT="$(cd "${OUT}" && pwd)"
mkdir -p "${SRC}" "${FX}" "${RES}"
git -C "${REPO_ROOT}" archive "${SHA}" | tar -x -C "${SRC}"
# git archive は全ファイルの mtime をコミット時刻にする。共有 CARGO_TARGET_DIR を使い回すと、
# 新しいコミットでビルドした後に古いコミットを展開した場合、cargo が「成果物より古い」ソースを
# 変更なしと誤判定して別コミットのビルド成果物を再利用する（ed2effa で観測）。展開直後に
# 全ファイルの mtime を現在時刻へ更新し、毎回 workspace のクレートを確実に再ビルドさせる。
find "${SRC}" -type f -exec touch {} +

cd "${SRC}"
export CARGO_TARGET_DIR="${BENCH_TARGET}"
cargo build --release --manifest-path crates/wire-server/Cargo.toml >"${TMP}/build.log" 2>&1 \
  || { tail -30 "${TMP}/build.log" >&2; exit 3; }
BIN="${BENCH_TARGET}/release/wire-server"
cargo build --release --manifest-path crates/engine/Cargo.toml --example seed_docs >"${TMP}/seed-build.log" 2>&1 \
  || { tail -30 "${TMP}/seed-build.log" >&2; exit 4; }
SEED="${BENCH_TARGET}/release/examples/seed_docs"
"${SEED}" seed "${FX}/docs25k.redb" 25000 128 >"${TMP}/seed.log" 2>&1 \
  || { tail -30 "${TMP}/seed.log" >&2; exit 4; }
"${SEED}" export "${FX}/docs25k.redb" "${FX}/docs25k.jsonl" >/dev/null
"${SEED}" queries 128 200 "${FX}/queries200.jsonl" >/dev/null

CROSSDB_SELF_BINARY="${BIN}" CROSSDB_SELF_PORT="${PORT}" \
  "${PY}" "${SRC}/scripts/crossdb_bench/run.py" --db self --config exact \
    --rows-file "${FX}/docs25k.redb" --queries-file "${FX}/queries200.jsonl" \
    --docs-file "${FX}/docs25k.jsonl" --out-dir "${RES}" >"${TMP}/stdout.log" 2>&1 \
  || { tail -30 "${TMP}/stdout.log" >&2; exit 5; }
cp "${RES}/self_exact.json" "${OUT}/self_exact.json"
sed -e 's/[[:space:]]*$//' "${TMP}/stdout.log" >"${OUT}/stdout.log"
echo "done ${SHA} -> ${OUT}"
