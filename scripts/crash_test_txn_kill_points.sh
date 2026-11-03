#!/usr/bin/env bash
# Issue #2416（親 TASK-332。対象ビヘイビア: PERSIST-1・SQL-31・TABLE-10。ポインタ:
# docs/spec/05-tasks.md TASK-332）の明示トランザクション・決定的 SIGKILL 回帰テスト。
#
# `crates/engine/examples/crash_tool_cross_table.rs` の `write-pause` サブコマンドで、
# 3 表（台帳・明細・バッチ）をまたぐ明示 Tx の「実行中（in-txn）」「COMMIT 直前
# （before-commit）」「COMMIT 直後（after-commit）」の各点で writer を停止させ、PAUSED 行を
# 確認してから SIGKILL する。再起動後の verify が破損 0・部分コミット 0 であり、行数・バッチ数が
# PAUSED 行の期待値と完全一致すること（途中・直前は何も残らず、直後は全部残る）を検証する。
# ランダム待機の kill 段（scripts/crash_test_cross_table.sh）は kill 位置を制御しないため、
# 本スクリプトが 3 点を毎回必ず観測する。観測できなかった点があれば失敗にする。
# Makefile の crash-test-txn-kill-points ターゲット・CI の crash-test-cross-table ジョブの
# step から呼ばれる（必須チェック名は不変）。
#
# 使い方: scripts/crash_test_txn_kill_points.sh [セット数（既定 3）]

set -u

SETS="${1:-3}"
if ! [[ "${SETS}" =~ ^[0-9]+$ ]] || [ "${SETS}" -lt 1 ]; then
  echo "ERROR: sets must be a positive integer, got: ${SETS}" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${REPO_ROOT}/target/release/examples/crash_tool_cross_table"

echo "building crash_tool_cross_table (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine --example crash_tool_cross_table); then
  echo "ERROR: failed to build crash_tool_cross_table" >&2
  exit 1
fi
if [ ! -x "${BIN}" ]; then
  echo "ERROR: crash_tool_cross_table binary not found at ${BIN}" >&2
  exit 1
fi

# オラクルが空振りしない（部分コミット・空 DB を検出できる）ことを先に確かめる。
SELF_CHECK_DIR="$(mktemp -d)"
if ! "${BIN}" self-check "${SELF_CHECK_DIR}" txn; then
  echo "ERROR: txn mode self-check failed" >&2
  rm -rf "${SELF_CHECK_DIR}"
  exit 1
fi
rm -rf "${SELF_CHECK_DIR}"

fail() {
  echo "ERROR: $*" >&2
  rm -rf "${WORKDIR:-}"
  trap - EXIT
  exit 1
}

# writer を確実に止めて回収する（失敗経路の後始末）。
reap() {
  kill -9 "$1" 2>/dev/null
  wait "$1" 2>/dev/null
}

observed_in_txn=0
observed_before_commit=0
observed_after_commit=0
completed=0

for set_no in $(seq 1 "${SETS}"); do
  WORKDIR="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '${WORKDIR}'" EXIT
  DB_PATH="${WORKDIR}/crash_test_txn_kill_points.redb"
  WRITE_LOG="${WORKDIR}/write.log"

  # 同じ DB で 3 点を順に実行する（直前の kill 後の状態から書き込みを再開できることも検証）。
  for point in in-txn before-commit after-commit; do
    echo "=== txn-kill-points set ${set_no}/${SETS} point ${point} ==="
    : > "${WRITE_LOG}"
    warmup=$((1 + RANDOM % 3))
    "${BIN}" write-pause "${DB_PATH}" "${point}" "${warmup}" >>"${WRITE_LOG}" 2>&1 &
    writer_pid=$!

    pat="^PAUSED point=${point} batch=[0-9]+ expect_rows=[0-9]+ expect_batches=[0-9]+$"
    waited=0
    paused=false
    while [ "${waited}" -lt 300 ]; do
      if grep -Eq "${pat}" "${WRITE_LOG}" 2>/dev/null; then
        paused=true
        break
      fi
      if ! kill -0 "${writer_pid}" 2>/dev/null; then
        break
      fi
      sleep 0.1
      waited=$((waited + 1))
    done
    if [ "${paused}" != "true" ]; then
      reap "${writer_pid}"
      cat "${WRITE_LOG}" >&2
      fail "writer did not reach the pause point (set ${set_no} point ${point})"
    fi

    paused_line="$(grep -E "${pat}" "${WRITE_LOG}" | tail -1)"
    # PAUSED が書き切られた最後の進行行であること（ack から永続性を推定していない証明）。
    last_line="$(grep -E '^(COMMITTED |PAUSED |IN_TXN )' "${WRITE_LOG}" | tail -1)"
    if [ "${last_line}" != "${paused_line}" ]; then
      reap "${writer_pid}"
      cat "${WRITE_LOG}" >&2
      fail "PAUSED is not the last progress line (set ${set_no} point ${point})"
    fi
    expect_rows="$(echo "${paused_line}" | sed -n 's/.* expect_rows=\([0-9]*\) .*/\1/p')"
    expect_batches="$(echo "${paused_line}" | sed -n 's/.* expect_batches=\([0-9]*\)$/\1/p')"
    paused_batch="$(echo "${paused_line}" | sed -n 's/.* batch=\([0-9]*\) .*/\1/p')"
    if ! [[ "${expect_rows}" =~ ^[0-9]+$ ]] || ! [[ "${expect_batches}" =~ ^[0-9]+$ ]] \
      || ! [[ "${paused_batch}" =~ ^[0-9]+$ ]]; then
      reap "${writer_pid}"
      fail "could not parse PAUSED line: ${paused_line}"
    fi
    # 停止中の Tx は COMMITTED を出していない（after-commit も ack 前に停止する）。
    if grep -q "^COMMITTED batch=${paused_batch} " "${WRITE_LOG}"; then
      reap "${writer_pid}"
      cat "${WRITE_LOG}" >&2
      fail "paused transaction was already acknowledged (set ${set_no} point ${point})"
    fi

    if ! kill -9 "${writer_pid}" 2>/dev/null; then
      cat "${WRITE_LOG}" >&2
      fail "kill -9 failed for writer pid ${writer_pid} (set ${set_no} point ${point})"
    fi
    wait "${writer_pid}" 2>/dev/null
    writer_status=$?
    if [ "${writer_status}" -ne 137 ]; then
      cat "${WRITE_LOG}" >&2
      fail "writer did not terminate via SIGKILL (wait exit status=${writer_status}, expected 137; set ${set_no} point ${point})"
    fi

    verify_output="$("${BIN}" verify "${DB_PATH}" txn)"
    echo "${verify_output}"
    rows="$(echo "${verify_output}" | sed -n 's/.* rows=\([0-9]*\).*/\1/p')"
    batches="$(echo "${verify_output}" | sed -n 's/.* batches=\([0-9]*\).*/\1/p')"
    if [[ "${verify_output}" != RESULT\ ok=true* ]] \
      || ! [[ "${rows}" =~ ^[0-9]+$ ]] || ! [[ "${batches}" =~ ^[0-9]+$ ]] \
      || [ "${rows}" -ne "${expect_rows}" ] || [ "${batches}" -ne "${expect_batches}" ]; then
      fail "verify mismatch at set ${set_no} point ${point}: expected rows=${expect_rows} batches=${expect_batches}, got: ${verify_output}"
    fi

    case "${point}" in
      in-txn) observed_in_txn=$((observed_in_txn + 1)) ;;
      before-commit) observed_before_commit=$((observed_before_commit + 1)) ;;
      after-commit) observed_after_commit=$((observed_after_commit + 1)) ;;
    esac
    completed=$((completed + 1))
  done

  rm -rf "${WORKDIR}"
  trap - EXIT
done

# 空虚な成功を拒否する: 3 点すべてをセット数だけ観測していること。
if [ "${observed_in_txn}" -ne "${SETS}" ] || [ "${observed_before_commit}" -ne "${SETS}" ] \
  || [ "${observed_after_commit}" -ne "${SETS}" ] || [ "${completed}" -ne $((3 * SETS)) ]; then
  echo "ERROR: kill points not fully observed (in-txn=${observed_in_txn} before-commit=${observed_before_commit} after-commit=${observed_after_commit} completed=${completed}, expected ${SETS} each)" >&2
  exit 1
fi

echo "crash_test_txn_kill_points.sh: OK (${SETS} sets x 3 points (in-txn, before-commit, after-commit) = ${completed} SIGKILL reps)"
