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
#
# Issue #3318（親 TASK-349。対象ビヘイビア: SQL-80・SQL-31・SQL-23・SQL-52・TABLE-22・TABLE-36・
# TABLE-44。ポインタ: docs/spec/05-tasks.md TASK-349）で、DDL（表の作成・一意索引・スカラー索引）と
# DML を混ぜた明示 Tx の段を足した（道具は `crates/engine/examples/crash_tool_txn_ddl.rs`）。
#   段 A: 上記の DML だけの 3 表 Tx の決定的 3 点（従来どおり）。
#   段 B: DDL を含む Tx の決定的 3 点。再起動後は台帳・カタログ・行・索引宣言・物理構造が
#         「全部ある」（after-commit）か「全部ない」（in-txn・before-commit）かのどちらかに完全一致する。
#   段 C: DDL を含む Tx をランダム待機で kill する（COMMIT の実行中を確率的に覆う）。最後の進行行が
#         COMMIT_SENT なら再起動後の Tx 数は ack 済み n か n+1、それ以外は n と完全一致。IN_TXN を
#         1 度も観測しなければ失敗にする。COMMIT_SENT での kill 回数は出力するだけで必須にしない
#         （COMMIT は短く、遅い runner では観測できず不安定になるため。redb commit の前後は段 B が
#         毎回必ず検査する。アサーションの弱体化ではない）。
# COMMIT 内部の決定的な同期点は、本番コードの変更と feature 付きの別ビルドが要るため採らない。
#
# Makefile の crash-test-txn-kill-points ターゲット・CI の crash-test-cross-table ジョブの
# step から呼ばれる（必須チェック名は不変）。
#
# 使い方: scripts/crash_test_txn_kill_points.sh [セット数（既定 3）] [段 C の反復回数（既定 20）]

# kill -9 と wait の 137 を扱うため set -e は使わない。
set -u

SETS="${1:-3}"
if ! [[ "${SETS}" =~ ^[0-9]+$ ]] || [ "${SETS}" -lt 1 ]; then
  echo "ERROR: sets must be a positive integer, got: ${SETS}" >&2
  exit 1
fi
ROUNDS="${2:-20}"
if ! [[ "${ROUNDS}" =~ ^[0-9]+$ ]] || [ "${ROUNDS}" -lt 1 ]; then
  echo "ERROR: rounds must be a positive integer, got: ${ROUNDS}" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${REPO_ROOT}/target/release/examples/crash_tool_cross_table"
TXN_BIN="${REPO_ROOT}/target/release/examples/crash_tool_txn_ddl"

echo "building crash_tool_cross_table and crash_tool_txn_ddl (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine \
  --example crash_tool_cross_table --example crash_tool_txn_ddl); then
  echo "ERROR: failed to build crash tools" >&2
  exit 1
fi
for b in "${BIN}" "${TXN_BIN}"; do
  if [ ! -x "${b}" ]; then
    echo "ERROR: crash tool binary not found at ${b}" >&2
    exit 1
  fi
done

# オラクルが空振りしない（部分コミット・空 DB を検出できる）ことを先に確かめる。
SELF_CHECK_DIR="$(mktemp -d)"
if ! "${BIN}" self-check "${SELF_CHECK_DIR}" txn; then
  echo "ERROR: txn mode self-check failed" >&2
  rm -rf "${SELF_CHECK_DIR}"
  exit 1
fi
rm -rf "${SELF_CHECK_DIR}"
SELF_CHECK_DIR="$(mktemp -d)"
if ! "${TXN_BIN}" self-check "${SELF_CHECK_DIR}"; then
  echo "ERROR: txn-ddl self-check failed" >&2
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

# ---------- 段 A: DML だけの 3 表 Tx の決定的 3 点 ----------
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

# ---------- 段 B: DDL を含む Tx の決定的 3 点（Issue #3318） ----------
b_in_txn=0
b_before_commit=0
b_after_commit=0
b_completed=0

for set_no in $(seq 1 "${SETS}"); do
  WORKDIR="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '${WORKDIR}'" EXIT
  DB_PATH="${WORKDIR}/crash_test_txn_ddl_kill_points.redb"
  WRITE_LOG="${WORKDIR}/write.log"

  # 同じ DB で 3 点を順に実行する（直前の kill 後の状態から DDL を含む Tx を再開できることも検証）。
  for point in in-txn before-commit after-commit; do
    echo "=== txn-ddl-kill-points set ${set_no}/${SETS} point ${point} ==="
    : > "${WRITE_LOG}"
    warmup=$((1 + RANDOM % 3))
    "${TXN_BIN}" write-pause "${DB_PATH}" "${point}" "${warmup}" >>"${WRITE_LOG}" 2>&1 &
    writer_pid=$!

    pat="^PAUSED point=${point} txn=[0-9]+ expect_txns=[0-9]+$"
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
      fail "ddl writer did not reach the pause point (set ${set_no} point ${point})"
    fi

    paused_line="$(grep -E "${pat}" "${WRITE_LOG}" | tail -1)"
    last_line="$(grep -E '^(COMMITTED |PAUSED |IN_TXN |COMMIT_SENT )' "${WRITE_LOG}" | tail -1)"
    if [ "${last_line}" != "${paused_line}" ]; then
      reap "${writer_pid}"
      cat "${WRITE_LOG}" >&2
      fail "PAUSED is not the last progress line (ddl set ${set_no} point ${point})"
    fi
    expect_txns="$(echo "${paused_line}" | sed -n 's/.* expect_txns=\([0-9]*\)$/\1/p')"
    paused_txn="$(echo "${paused_line}" | sed -n 's/.* txn=\([0-9]*\) .*/\1/p')"
    if ! [[ "${expect_txns}" =~ ^[0-9]+$ ]] || ! [[ "${paused_txn}" =~ ^[0-9]+$ ]]; then
      reap "${writer_pid}"
      fail "could not parse PAUSED line: ${paused_line}"
    fi
    # 停止中の Tx は COMMITTED を出していない（after-commit も ack 前に停止する）。
    if grep -q "^COMMITTED txn=${paused_txn} " "${WRITE_LOG}"; then
      reap "${writer_pid}"
      cat "${WRITE_LOG}" >&2
      fail "paused ddl transaction was already acknowledged (set ${set_no} point ${point})"
    fi

    if ! kill -9 "${writer_pid}" 2>/dev/null; then
      cat "${WRITE_LOG}" >&2
      fail "kill -9 failed for ddl writer pid ${writer_pid} (set ${set_no} point ${point})"
    fi
    wait "${writer_pid}" 2>/dev/null
    writer_status=$?
    if [ "${writer_status}" -ne 137 ]; then
      cat "${WRITE_LOG}" >&2
      fail "ddl writer did not terminate via SIGKILL (wait exit status=${writer_status}, expected 137; set ${set_no} point ${point})"
    fi

    verify_output="$("${TXN_BIN}" verify "${DB_PATH}")"
    echo "${verify_output}"
    txns="$(echo "${verify_output}" | sed -n 's/^RESULT ok=true txns=\([0-9]*\)$/\1/p')"
    if ! [[ "${txns}" =~ ^[0-9]+$ ]] || [ "${txns}" -ne "${expect_txns}" ]; then
      fail "ddl verify mismatch at set ${set_no} point ${point}: expected txns=${expect_txns}, got: ${verify_output}"
    fi

    case "${point}" in
      in-txn) b_in_txn=$((b_in_txn + 1)) ;;
      before-commit) b_before_commit=$((b_before_commit + 1)) ;;
      after-commit) b_after_commit=$((b_after_commit + 1)) ;;
    esac
    b_completed=$((b_completed + 1))
  done

  rm -rf "${WORKDIR}"
  trap - EXIT
done

if [ "${b_in_txn}" -ne "${SETS}" ] || [ "${b_before_commit}" -ne "${SETS}" ] \
  || [ "${b_after_commit}" -ne "${SETS}" ] || [ "${b_completed}" -ne $((3 * SETS)) ]; then
  echo "ERROR: ddl kill points not fully observed (in-txn=${b_in_txn} before-commit=${b_before_commit} after-commit=${b_after_commit} completed=${b_completed}, expected ${SETS} each)" >&2
  exit 1
fi

# ---------- 段 C: DDL を含む Tx のランダム kill（Issue #3318） ----------
c_in_txn=0
c_commit_sent=0
c_between=0
prev_txns=0

WORKDIR="$(mktemp -d)"
# shellcheck disable=SC2064
trap "rm -rf '${WORKDIR}'" EXIT
DB_PATH="${WORKDIR}/crash_test_txn_ddl_random.redb"
WRITE_LOG="${WORKDIR}/write.log"

for round in $(seq 1 "${ROUNDS}"); do
  : > "${WRITE_LOG}"
  "${TXN_BIN}" write "${DB_PATH}" >>"${WRITE_LOG}" 2>&1 &
  writer_pid=$!

  # 少なくとも 1 件 ack されるまで待つ（再開時の検査・起動の時間は kill の対象にしない）。
  waited=0
  started=false
  while [ "${waited}" -lt 300 ]; do
    if grep -Eq '^COMMITTED txn=[0-9]+ txns=[0-9]+$' "${WRITE_LOG}" 2>/dev/null; then
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
    reap "${writer_pid}"
    cat "${WRITE_LOG}" >&2
    fail "ddl writer did not start committing (round ${round})"
  fi

  sleep "0.$(printf '%03d' $((10 + RANDOM % 291)))"
  if ! kill -9 "${writer_pid}" 2>/dev/null; then
    cat "${WRITE_LOG}" >&2
    fail "kill -9 failed for ddl writer pid ${writer_pid} (round ${round})"
  fi
  wait "${writer_pid}" 2>/dev/null
  writer_status=$?
  if [ "${writer_status}" -ne 137 ]; then
    cat "${WRITE_LOG}" >&2
    fail "ddl writer did not terminate via SIGKILL (wait exit status=${writer_status}, expected 137; round ${round})"
  fi

  # 書き切られた進行行だけを見る（kill で途中切れの行は数えない）。
  last_line="$(grep -E '^(IN_TXN txn=[0-9]+|COMMIT_SENT txn=[0-9]+|COMMITTED txn=[0-9]+ txns=[0-9]+)$' "${WRITE_LOG}" | tail -1)"
  acked="$(grep -E '^COMMITTED txn=[0-9]+ txns=[0-9]+$' "${WRITE_LOG}" | tail -1 | sed -n 's/.* txns=\([0-9]*\)$/\1/p')"
  if ! [[ "${acked}" =~ ^[0-9]+$ ]]; then
    fail "could not parse the last COMMITTED line (round ${round})"
  fi

  verify_output="$("${TXN_BIN}" verify "${DB_PATH}")"
  txns="$(echo "${verify_output}" | sed -n 's/^RESULT ok=true txns=\([0-9]*\)$/\1/p')"
  if ! [[ "${txns}" =~ ^[0-9]+$ ]]; then
    echo "${verify_output}" >&2
    fail "ddl verify failed (round ${round})"
  fi

  case "${last_line}" in
    COMMIT_SENT*)
      # COMMIT の実行中: ack 済み n か n+1（部分状態は verify が拒否する）。
      if [ "${txns}" -ne "${acked}" ] && [ "${txns}" -ne $((acked + 1)) ]; then
        fail "txns=${txns} is neither ${acked} nor $((acked + 1)) after a kill during COMMIT (round ${round})"
      fi
      c_commit_sent=$((c_commit_sent + 1))
      ;;
    IN_TXN*)
      if [ "${txns}" -ne "${acked}" ]; then
        fail "txns=${txns} != ${acked} after a kill inside a transaction (round ${round})"
      fi
      c_in_txn=$((c_in_txn + 1))
      ;;
    *)
      if [ "${txns}" -ne "${acked}" ]; then
        fail "txns=${txns} != ${acked} after a kill between transactions (round ${round})"
      fi
      c_between=$((c_between + 1))
      ;;
  esac
  # ack 済みの Tx が消えていない（単調増加）。
  if [ "${txns}" -lt "${prev_txns}" ]; then
    fail "txns went backwards: ${prev_txns} -> ${txns} (round ${round})"
  fi
  prev_txns="${txns}"
done

rm -rf "${WORKDIR}"
trap - EXIT

# 空虚な成功を拒否する: Tx の途中の kill を 1 度は観測していること。
if [ "${c_in_txn}" -lt 1 ]; then
  echo "ERROR: no kill landed inside a ddl transaction (in-txn=${c_in_txn} commit-sent=${c_commit_sent} between=${c_between})" >&2
  exit 1
fi

echo "crash_test_txn_kill_points.sh: OK (stage A: ${SETS} sets x 3 points = ${completed} SIGKILL reps; stage B (DDL txn): ${SETS} sets x 3 points = ${b_completed} SIGKILL reps; stage C (DDL txn random): ${ROUNDS} rounds, kills in-txn=${c_in_txn} commit-sent=${c_commit_sent} between=${c_between}, final txns=${prev_txns})"
