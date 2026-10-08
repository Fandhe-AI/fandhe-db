#!/usr/bin/env bash
# Issue #1131（分割実行 DML の crash 耐性。ポインタ: RECOVER-11・RECOVER-12・PERSIST-1。
# ADR docs/design/partitioned-dml.md 9.3 節）の回帰テスト。
#
# `crates/engine/examples/crash_tool_partitioned_dml.rs`（init/write/verify/finish）を使い、
# チャンクごとに commit する分割実行（述語形 DELETE／UPDATE）を実行中のプロセスを SIGKILL
# し、再オープン後に 9.3 節の 4 基準（チャンク原子性・前方一致・ちょうど 1 回・台帳エントリ）
# を検証する。既存の crash-test 系（1 トランザクションの部分 commit が 0 件）とは別基準で、
# それらは変更しない。奇数セットは delete、偶数セットは update を通す。
# Makefile の crash-test-partitioned-dml ターゲット・CI の同名ジョブから呼ばれる。
#
# 使い方: scripts/crash_test_partitioned_dml.sh [セット数（既定 2）] [反復回数（既定 10）]

set -u

# `kill -9` と `wait`（137）は set -e と相性が悪いため、結果は都度 fail-closed に明示チェックする。

SETS="${1:-2}"
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
BIN="${REPO_ROOT}/target/release/examples/crash_tool_partitioned_dml"

echo "building crash_tool_partitioned_dml (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine --example crash_tool_partitioned_dml); then
  echo "ERROR: failed to build crash_tool_partitioned_dml" >&2
  exit 1
fi
if [ ! -x "${BIN}" ]; then
  echo "ERROR: crash_tool_partitioned_dml binary not found at ${BIN}" >&2
  exit 1
fi

# 進捗行が出るまでの最大待機（5ms 単位。6000 = 30 秒）。
START_TIMEOUT_TICKS=6000

WORKDIR=""

fail() {
  echo "ERROR: $1" >&2
  if [ -n "${WORKDIR}" ]; then rm -rf "${WORKDIR}"; fi
  trap - EXIT
  exit 1
}

completed=0

for set_no in $(seq 1 "${SETS}"); do
  echo "### set ${set_no}/${SETS} ==="
  if [ $((set_no % 2)) -eq 1 ]; then MODE="delete"; else MODE="update"; fi

  WORKDIR="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '${WORKDIR}'" EXIT
  DB_PATH="${WORKDIR}/crash_test_partitioned_dml.redb"
  WRITE_LOG="${WORKDIR}/write.log"

  init_output="$("${BIN}" init "${DB_PATH}" "${MODE}")" || fail "init failed (set ${set_no}): ${init_output}"
  echo "${init_output} mode=${MODE}"

  last_rows=0
  saw_interrupted=false

  for i in $(seq 1 "${ITERATIONS}"); do
    echo "=== set ${set_no}/${SETS} (${MODE}) iteration ${i}/${ITERATIONS} ==="

    : > "${WRITE_LOG}"
    "${BIN}" write "${DB_PATH}" "${MODE}" >>"${WRITE_LOG}" 2>&1 &
    writer_pid=$!

    waited=0
    started=false
    while [ "${waited}" -lt "${START_TIMEOUT_TICKS}" ]; do
      if grep -Eq '^(PROGRESS status=[a-z]+ rows=[0-9]+|DONE rows=[0-9]+|ALREADY_COMPLETED)$' "${WRITE_LOG}" 2>/dev/null; then
        started=true
        break
      fi
      if ! kill -0 "${writer_pid}" 2>/dev/null; then
        break
      fi
      sleep 0.005
      waited=$((waited + 1))
    done
    if [ "${started}" != "true" ]; then
      kill -9 "${writer_pid}" 2>/dev/null
      wait "${writer_pid}" 2>/dev/null
      cat "${WRITE_LOG}" >&2
      fail "writer did not report progress in time (set ${set_no} iteration ${i})"
    fi

    # チャンク commit の途中に kill が当たる確率を上げる短いランダム待機（1〜40ms）。
    wait_ms=$((1 + RANDOM % 40))
    sleep "0.$(printf '%03d' "${wait_ms}")"

    if ! kill -9 "${writer_pid}" 2>/dev/null; then
      cat "${WRITE_LOG}" >&2
      fail "kill -9 failed for writer pid ${writer_pid} (set ${set_no} iteration ${i}); the writer exited on its own"
    fi
    wait "${writer_pid}" 2>/dev/null
    writer_status=$?
    if [ "${writer_status}" -ne 137 ]; then
      cat "${WRITE_LOG}" >&2
      fail "writer did not terminate via SIGKILL (status=${writer_status}, expected 137; set ${set_no} iteration ${i})"
    fi

    verify_output="$("${BIN}" verify "${DB_PATH}" "${MODE}")"
    echo "${verify_output}"
    if [[ "${verify_output}" != RESULT\ ok=true* ]]; then
      fail "verify failed at set ${set_no} iteration ${i}: ${verify_output}"
    fi
    status="$(echo "${verify_output}" | sed -n 's/.* status=\([a-z]*\).*/\1/p')"
    rows="$(echo "${verify_output}" | sed -n 's/.* rows=\([0-9]*\)$/\1/p')"
    if ! [[ "${rows}" =~ ^[0-9]+$ ]]; then
      fail "could not parse rows from verify output: ${verify_output}"
    fi

    # 行末固定（`$`）で、kill 前に完全に書き切られた最後の PROGRESS／DONE 行の rows を acked とする
    # （切れた行は採用しない。再起動後の rows が acked を下回れば確認済み commit の喪失）。
    acked="$(grep -Eo '^(PROGRESS status=[a-z]+|DONE) rows=[0-9]+$' "${WRITE_LOG}" | tail -1 | sed -n 's/.* rows=\([0-9]*\)$/\1/p')"
    if [ -n "${acked}" ] && [ "${rows}" -lt "${acked}" ]; then
      fail "rows ${rows} is below the last acknowledged progress ${acked}, data loss suspected (set ${set_no} iteration ${i})"
    fi
    if [ "${rows}" -lt "${last_rows}" ]; then
      fail "rows decreased across restarts (${last_rows} -> ${rows}) (set ${set_no} iteration ${i})"
    fi
    last_rows="${rows}"
    completed=$((completed + 1))

    if [ "${status}" = "interrupted" ]; then
      saw_interrupted=true
    fi
    # 完了済みなら以降の反復は再送が即座に 23505 になるだけなので打ち切る。
    if [ "${status}" = "completed" ]; then
      break
    fi
  done

  # 空虚な成功（kill が毎回完了後に当たり、中断状態を一度も検証できていない）を拒否する。
  if [ "${saw_interrupted}" != "true" ]; then
    fail "set ${set_no} never observed an interrupted job; increase ROWS in crash_tool_partitioned_dml.rs so the job outlasts the kill delay"
  fi

  finish_output="$("${BIN}" finish "${DB_PATH}" "${MODE}")"
  echo "${finish_output}"
  if [[ "${finish_output}" != RESULT\ ok=true* ]]; then
    fail "finish failed at set ${set_no}: ${finish_output}"
  fi

  rm -rf "${WORKDIR}"
  trap - EXIT
done

echo "crash_test_partitioned_dml.sh: OK (${SETS} sets, ${completed} SIGKILL reps)"
