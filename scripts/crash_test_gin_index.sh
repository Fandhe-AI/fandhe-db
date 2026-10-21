#!/usr/bin/env bash
# Issue #2209（INDEX-8・PERSIST-1。設計は docs/design/gin-jsonb-index.md D11）の GIN 相当索引の
# クラッシュ耐性回帰テスト。
#
# `crates/engine/examples/crash_tool_gin_index.rs`（write/verify/self-check サブコマンド）を使い、
# 行・ポスティング・マーカー・逆引きを同一 write トランザクションで書いているプロセスを SIGKILL で
# 強制終了させ、再オープン後に「索引が行と一致する（欠落 0・stale 0）」「索引経由の読み取り結果が
# 全走査（GIN なしの双子表）と一致する」ことを検証する反復を行う。
# - 奇数セット: `dml` モード（複数テナントの INSERT・UPDATE・DELETE を 1 バッチ 1 txn で反復）
# - 偶数セット: `ddl` モード（DROP INDEX と CREATE INDEX の反復。構築・破棄の途中の kill を狙う）
# 構成（セット × 反復回数の 2 段構成。セットごとに新規 DB を作り直す）は
# `scripts/crash_test_sequence.sh` と同じで、冒頭で `self-check`（負の対照）を実行する。
# Makefile の crash-test-gin-index ターゲット・CI の crash-test-gin-index ジョブから呼ばれる。
#
# 使い方: scripts/crash_test_gin_index.sh [セット数（既定 4）] [反復回数（既定 10）]

set -u

# `kill -9` は 137、`wait` はシグナル終了したプロセスの終了コードを返すため、
# set -e はそれらと相性が悪い。検証結果は都度 fail-closed に明示チェックする
# （既存の crash_test_*.sh と同方針）。

SETS="${1:-4}"
ITERATIONS="${2:-10}"
if ! [[ "${SETS}" =~ ^[0-9]+$ ]] || [ "${SETS}" -lt 1 ]; then
  echo "ERROR: sets must be a positive integer, got: ${SETS}" >&2
  exit 1
fi
if ! [[ "${ITERATIONS}" =~ ^[0-9]+$ ]] || [ "${ITERATIONS}" -lt 1 ]; then
  echo "ERROR: iterations must be a positive integer, got: ${ITERATIONS}" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${REPO_ROOT}/target/release/examples/crash_tool_gin_index"

echo "building crash_tool_gin_index (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine --example crash_tool_gin_index); then
  echo "ERROR: failed to build crash_tool_gin_index" >&2
  exit 1
fi
if [ ! -x "${BIN}" ]; then
  echo "ERROR: crash_tool_gin_index binary not found at ${BIN}" >&2
  exit 1
fi

# 負の対照: テナント A のポスティングを全削除した DB で、オラクルが欠落を検出できること。
SELF_DIR="$(mktemp -d)"
# shellcheck disable=SC2064
trap "rm -rf '${SELF_DIR}'" EXIT
self_output="$("${BIN}" self-check "${SELF_DIR}/self_check.redb")"
echo "${self_output}"
rm -rf "${SELF_DIR}"
trap - EXIT
if [[ "${self_output}" != SELF-CHECK\ ok=true* ]]; then
  echo "ERROR: self-check failed: ${self_output}" >&2
  exit 1
fi

# 進捗行（COMMITTED ...）が出るまでの最大待機（100ms 単位。値 300 = 30 秒）。実際に
# 書き込みが始まっていない状態で kill しても回帰テストとして意味が無いため、開始同期を取る。
WRITE_START_TIMEOUT_TENTHS=300

total_iterations=$((SETS * ITERATIONS))
completed=0

for set_no in $(seq 1 "${SETS}"); do
  if [ $((set_no % 2)) -eq 1 ]; then
    MODE="dml"
  else
    MODE="ddl"
  fi
  echo "### set ${set_no}/${SETS} mode=${MODE} ==="

  WORKDIR="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '${WORKDIR}'" EXIT
  DB_PATH="${WORKDIR}/crash_test_gin_index.redb"
  WRITE_LOG="${WORKDIR}/write.log"

  last_rows=0

  for i in $(seq 1 "${ITERATIONS}"); do
    echo "=== set ${set_no}/${SETS} (${MODE}) iteration ${i}/${ITERATIONS} ==="

    : > "${WRITE_LOG}"
    "${BIN}" write "${DB_PATH}" "${MODE}" >>"${WRITE_LOG}" 2>&1 &
    writer_pid=$!

    # 書き込みが実際に進み始めるまでポーリングする（タイムアウト付き。fail-closed）。
    waited=0
    started=false
    while [ "${waited}" -lt "${WRITE_START_TIMEOUT_TENTHS}" ]; do
      if grep -q "^COMMITTED " "${WRITE_LOG}" 2>/dev/null; then
        started=true
        break
      fi
      if ! kill -0 "${writer_pid}" 2>/dev/null; then
        break
      fi
      sleep 0.1
      waited=$((waited + 1))
    done

    if [ "${started}" != "true" ]; then
      timeout_ms=$((WRITE_START_TIMEOUT_TENTHS * 100))
      echo "ERROR: writer did not report progress within ${timeout_ms}ms (set ${set_no} iteration ${i})" >&2
      kill -9 "${writer_pid}" 2>/dev/null
      wait "${writer_pid}" 2>/dev/null
      cat "${WRITE_LOG}" >&2
      rm -rf "${WORKDIR}"
      trap - EXIT
      exit 1
    fi

    # コミット中・索引構築中に kill が当たる確率を上げるための短いランダム待機（10〜300ms）。
    wait_ms=$((10 + RANDOM % 291))
    sleep "0.$(printf '%03d' "${wait_ms}")"

    if ! kill -9 "${writer_pid}" 2>/dev/null; then
      echo "ERROR: kill -9 failed for writer pid ${writer_pid} at set ${set_no} iteration ${i} (writer may have already exited on its own); this iteration cannot verify SIGKILL crash resilience" >&2
      cat "${WRITE_LOG}" >&2
      rm -rf "${WORKDIR}"
      trap - EXIT
      exit 1
    fi

    wait "${writer_pid}" 2>/dev/null
    writer_status=$?
    # SIGKILL で終了したプロセスに対して `wait` は 137 を返す。それ以外は SIGKILL を
    # 検証できていない反復であり、テストの目的を満たさないため拒否する。
    if [ "${writer_status}" -ne 137 ]; then
      echo "ERROR: writer pid ${writer_pid} did not terminate via SIGKILL at set ${set_no} iteration ${i} (wait exit status=${writer_status}, expected 137)" >&2
      cat "${WRITE_LOG}" >&2
      rm -rf "${WORKDIR}"
      trap - EXIT
      exit 1
    fi

    verify_output="$("${BIN}" verify "${DB_PATH}" "${MODE}")"
    echo "${verify_output}"

    if [[ "${verify_output}" != RESULT\ ok=true* ]]; then
      echo "ERROR: verify failed at set ${set_no} iteration ${i}: ${verify_output}" >&2
      rm -rf "${WORKDIR}"
      trap - EXIT
      exit 1
    fi

    rows="$(echo "${verify_output}" | sed -n 's/.*rows=\([0-9]*\).*/\1/p')"
    if ! [[ "${rows}" =~ ^[0-9]+$ ]]; then
      echo "ERROR: could not parse row count from verify output: ${verify_output}" >&2
      rm -rf "${WORKDIR}"
      trap - EXIT
      exit 1
    fi

    # write.log の最後に完全に書き切られた `COMMITTED <label>=<n> rows=<alive>` 行から、kill 前に
    # 確認済みだった行数（acked）を取り出す。`$` で終端を固定しているため、kill が出力の途中で当たって
    # 行が切れた場合は 1 つ前の完全な行が採用される（acked は実際より小さくなっても大きくならない）。
    acked="$(grep -o '^COMMITTED [a-z]*=[0-9]* rows=[0-9]*$' "${WRITE_LOG}" | tail -1 | sed -n 's/.* rows=\([0-9]*\)$/\1/p')"
    if [ -n "${acked}" ] && [ "${rows}" -lt "${acked}" ]; then
      echo "ERROR: row count ${rows} is below the last acknowledged commit (rows=${acked} from write.log), data loss suspected (set ${set_no} iteration ${i})" >&2
      rm -rf "${WORKDIR}"
      trap - EXIT
      exit 1
    fi

    # dml は 1 バッチごとに行が増えるため、COMMITTED を出した後の再起動ごとに rows は真に増える。
    # ddl は行数が一定（索引の作り直しだけ）なので増加は要求しない。
    if [ "${MODE}" = "dml" ] && [ "${rows}" -le "${last_rows}" ]; then
      echo "ERROR: row count did not increase across restarts (${last_rows} -> ${rows}) despite a committed batch, data loss suspected (set ${set_no} iteration ${i})" >&2
      rm -rf "${WORKDIR}"
      trap - EXIT
      exit 1
    fi
    last_rows="${rows}"
    completed=$((completed + 1))
  done

  # 空虚な成功（DB 未作成・0 行のまま検証をすり抜ける）を拒否する。
  if [ "${last_rows}" -le 0 ]; then
    echo "ERROR: final row count for set ${set_no} is not positive (${last_rows})" >&2
    rm -rf "${WORKDIR}"
    trap - EXIT
    exit 1
  fi

  rm -rf "${WORKDIR}"
  trap - EXIT
done

if [ "${completed}" -ne "${total_iterations}" ]; then
  echo "ERROR: completed ${completed} iterations, expected ${total_iterations}" >&2
  exit 1
fi

echo "crash_test_gin_index.sh: OK (${SETS} sets x ${ITERATIONS} iterations = ${completed} SIGKILL reps)"
