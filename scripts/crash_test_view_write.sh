#!/usr/bin/env bash
# Issue #1674（TABLE-43）の更新可能ビュー書き込み・crash 耐性検査。
#
# `crates/engine/examples/crash_tool_view_write.rs`（write/verify/self-check サブコマンド）を使い、
# ビュー経由の INSERT・UPDATE・DELETE・明示トランザクションを発行し続けるプロセスを SIGKILL で
# 強制終了させ、再オープン後に次を検証する反復を行う。
#   - 台帳（operation_id）の適用済み操作が接頭辞で、明示トランザクションの 2 文が原子的であること
#   - 表の内容が、その接頭辞をモデルで再生した結果と完全に一致すること（二重適用・欠落・
#     CHECK OPTION の迂回・他テナント行の改変が無いこと）
#   - 再起動したプロセスが最後の適用済み操作を再送して、ビュー経由は 23505・基表への直接送信は
#     22023 になること（writer が自己検査し、外れれば writer が非 137 で終わり fail-closed になる）
# 反復の前に 1 回だけ `self-check`（負の対照: 壊れた DB を verify が必ず拒否すること）を実行し、
# 「何も無いから通るだけ」の空虚な成功を排除する。
# 骨格（セット × 反復、開始同期、ランダム待機、kill 失敗時の fail-closed）は
# `scripts/crash_test_temp_table.sh` と同じ。Makefile の crash-test-view-write ターゲット・
# CI の crash-test-view-write ジョブから呼ばれる。
#
# 使い方: scripts/crash_test_view_write.sh [セット数（既定 2）] [反復回数（既定 10）]

set -u

# `kill -9` は 137、`wait` はシグナル終了したプロセスの終了コードを返すため、
# set -e はそれらと相性が悪い。検証結果は都度 fail-closed に明示チェックする。

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
BIN="${REPO_ROOT}/target/release/examples/crash_tool_view_write"

echo "building crash_tool_view_write (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine --example crash_tool_view_write); then
    echo "ERROR: failed to build crash_tool_view_write" >&2
    exit 1
fi
if [ ! -x "${BIN}" ]; then
    echo "ERROR: crash_tool_view_write binary not found at ${BIN}" >&2
    exit 1
fi

WORKDIR=""
cleanup() {
    if [ -n "${WORKDIR}" ]; then
        rm -rf "${WORKDIR}"
    fi
}
trap cleanup EXIT

fail() {
    echo "ERROR: $*" >&2
    exit 1
}

# 負の対照（壊れた DB を verify が拒否できること）。
SELFCHECK_DIR="$(mktemp -d)"
if ! "${BIN}" self-check "${SELFCHECK_DIR}"; then
    rm -rf "${SELFCHECK_DIR}"
    fail "self-check (negative control) failed"
fi
rm -rf "${SELFCHECK_DIR}"

# 進捗行（COMMITTED ...）が出るまでの最大待機（100ms 単位。値 300 = 30 秒）。
WRITE_START_TIMEOUT_TENTHS=300

total_iterations=$((SETS * ITERATIONS))
completed=0

for set_no in $(seq 1 "${SETS}"); do
    echo "### set ${set_no}/${SETS} ==="

    WORKDIR="$(mktemp -d)"
    DB_PATH="${WORKDIR}/crash_test_view_write.redb"
    WRITE_LOG="${WORKDIR}/write.log"

    last_applied=0

    for i in $(seq 1 "${ITERATIONS}"); do
        echo "=== set ${set_no}/${SETS} iteration ${i}/${ITERATIONS} ==="

        : > "${WRITE_LOG}"
        "${BIN}" write "${DB_PATH}" >>"${WRITE_LOG}" 2>&1 &
        writer_pid=$!

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
            kill -9 "${writer_pid}" 2>/dev/null
            wait "${writer_pid}" 2>/dev/null
            cat "${WRITE_LOG}" >&2
            fail "writer did not report progress within ${timeout_ms}ms (set ${set_no} iteration ${i})"
        fi

        # kill が文の途中・明示トランザクションの COMMIT 前後に当たる確率を上げる
        # 短いランダム待機（10〜300ms）。
        wait_ms=$((10 + RANDOM % 291))
        sleep "0.$(printf '%03d' "${wait_ms}")"

        if ! kill -9 "${writer_pid}" 2>/dev/null; then
            cat "${WRITE_LOG}" >&2
            fail "kill -9 failed for writer pid ${writer_pid} at set ${set_no} iteration ${i} (writer may have exited on its own)"
        fi

        wait "${writer_pid}" 2>/dev/null
        writer_status=$?
        if [ "${writer_status}" -ne 137 ]; then
            cat "${WRITE_LOG}" >&2
            fail "writer pid ${writer_pid} did not terminate via SIGKILL at set ${set_no} iteration ${i} (wait exit status=${writer_status}, expected 137)"
        fi

        verify_output="$("${BIN}" verify "${DB_PATH}")"
        echo "${verify_output}"

        if [[ "${verify_output}" != RESULT\ ok=true* ]]; then
            fail "verify failed at set ${set_no} iteration ${i}: ${verify_output}"
        fi

        applied="$(echo "${verify_output}" | sed -n 's/.*applied=\([0-9]*\).*/\1/p')"
        if ! [[ "${applied}" =~ ^[0-9]+$ ]]; then
            fail "could not parse applied count from verify output: ${verify_output}"
        fi

        # kill 前に ack 済みだった適用済み操作数を下回ってはならない（data loss 検出）。
        # in-flight の文（最大で明示トランザクションの 2 操作）だけ先へ進んでいてよい。
        acked="$(grep -o '^COMMITTED applied=[0-9]*$' "${WRITE_LOG}" | tail -1 | sed -n 's/.*applied=\([0-9]*\)$/\1/p')"
        if ! [[ "${acked}" =~ ^[0-9]+$ ]]; then
            fail "could not parse the last acknowledged count from the writer log (set ${set_no} iteration ${i})"
        fi
        if [ "${applied}" -lt "${acked}" ]; then
            fail "applied ${applied} is below the last acknowledged commit (${acked}), data loss suspected (set ${set_no} iteration ${i})"
        fi
        if [ "${applied}" -gt $((acked + 2)) ]; then
            fail "applied ${applied} is more than 2 ops past the last acknowledged commit (${acked}) (set ${set_no} iteration ${i})"
        fi

        if [ "${applied}" -le "${last_applied}" ]; then
            fail "applied count did not increase across restarts (${last_applied} -> ${applied}) despite a committed op (set ${set_no} iteration ${i})"
        fi
        last_applied="${applied}"
        completed=$((completed + 1))
    done

    if [ "${last_applied}" -le 0 ]; then
        fail "final applied count for set ${set_no} is not positive (${last_applied})"
    fi

    rm -rf "${WORKDIR}"
    WORKDIR=""
done

if [ "${completed}" -ne "${total_iterations}" ]; then
    fail "completed ${completed} iterations, expected ${total_iterations}"
fi

echo "crash_test_view_write.sh: OK (${SETS} sets x ${ITERATIONS} iterations = ${completed} SIGKILL reps)"
