#!/usr/bin/env bash
# Issue #1685（TABLE-44 (2)）の一時表・異常終了後残骸検査。
#
# `crates/engine/examples/crash_tool_temp_table.rs`（write/verify/self-check サブコマンド）を使い、
# 一時表（接続ごとのインメモリ領域）を生かしたまま永続表へ書き込んでいるプロセスを SIGKILL で
# 強制終了させ、再オープン後に次を検証する反復を行う。
#   - 永続カタログ・redb の生テーブルに一時表・一時索引由来の痕跡が無いこと
#   - 永続表へ一時表の書き込みが漏れていないこと
#   - DB ファイルのディレクトリに DB ファイル以外が無いこと
#   - 新しいセッションから一時表が見えず、同名で作り直すと空であること
# あわせて、専用の空 TMPDIR（mktemp -d）を writer / verify に与え、全反復の後にその
# ディレクトリが空であること（一時ストアがディスクへ spill しないこと）を確かめる。システムの
# /tmp を走査・一括削除はしない（`scripts/check_tmp_leak.sh` と同方針）。
# 反復の前に 1 回だけ `self-check`（負の対照: 壊れた DB を verify が必ず拒否すること）を実行し、
# 「何も無いから通るだけ」の空虚な成功を排除する。
# 骨格（セット × 反復、開始同期、ランダム待機、kill 失敗時の fail-closed）は
# `scripts/crash_test_unique_index.sh` と同じ。Makefile の crash-test-temp-table ターゲット・
# CI の crash-test-temp-table ジョブから呼ばれる。
#
# 使い方: scripts/crash_test_temp_table.sh [セット数（既定 2）] [反復回数（既定 10）]

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
BIN="${REPO_ROOT}/target/release/examples/crash_tool_temp_table"

echo "building crash_tool_temp_table (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine --example crash_tool_temp_table); then
    echo "ERROR: failed to build crash_tool_temp_table" >&2
    exit 1
fi
if [ ! -x "${BIN}" ]; then
    echo "ERROR: crash_tool_temp_table binary not found at ${BIN}" >&2
    exit 1
fi

# 一時領域の残置検出用ディレクトリ（writer / verify / self-check の TMPDIR）。
# DB と作業ログはここに置かない（空であることが検査条件のため）。
TMP_PROBE="$(mktemp -d)"
WORKDIR=""
cleanup() {
    rm -rf "${TMP_PROBE}"
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
if ! TMPDIR="${TMP_PROBE}" "${BIN}" self-check "${SELFCHECK_DIR}"; then
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
    # データディレクトリ検査のため、DB 専用ディレクトリとログ置き場を分ける。
    mkdir "${WORKDIR}/db" "${WORKDIR}/log"
    DB_PATH="${WORKDIR}/db/crash_test_temp_table.redb"
    WRITE_LOG="${WORKDIR}/log/write.log"

    last_rows=0

    for i in $(seq 1 "${ITERATIONS}"); do
        echo "=== set ${set_no}/${SETS} iteration ${i}/${ITERATIONS} ==="

        : > "${WRITE_LOG}"
        TMPDIR="${TMP_PROBE}" "${BIN}" write "${DB_PATH}" >>"${WRITE_LOG}" 2>&1 &
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

        # kill が一時側の書き込みトランザクション中・永続コミット中に当たる確率を上げる
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

        verify_output="$(TMPDIR="${TMP_PROBE}" "${BIN}" verify "${DB_PATH}")"
        echo "${verify_output}"

        if [[ "${verify_output}" != RESULT\ ok=true* ]]; then
            fail "verify failed at set ${set_no} iteration ${i}: ${verify_output}"
        fi

        rows="$(echo "${verify_output}" | sed -n 's/.*rows=\([0-9]*\).*/\1/p')"
        if ! [[ "${rows}" =~ ^[0-9]+$ ]]; then
            fail "could not parse row count from verify output: ${verify_output}"
        fi

        # kill 前に確認済みだった永続コミット行数を下回ってはならない（data loss 検出）。
        acked="$(grep -o '^COMMITTED batch=[0-9]* rows=[0-9]*$' "${WRITE_LOG}" | tail -1 | sed -n 's/.*rows=\([0-9]*\)$/\1/p')"
        if [ -n "${acked}" ] && [ "${rows}" -lt "${acked}" ]; then
            fail "row count ${rows} is below the last acknowledged commit (rows=${acked}), data loss suspected (set ${set_no} iteration ${i})"
        fi

        if [ "${rows}" -le "${last_rows}" ]; then
            fail "row count did not increase across restarts (${last_rows} -> ${rows}) despite a committed batch (set ${set_no} iteration ${i})"
        fi
        last_rows="${rows}"
        completed=$((completed + 1))
    done

    if [ "${last_rows}" -le 0 ]; then
        fail "final row count for set ${set_no} is not positive (${last_rows})"
    fi

    rm -rf "${WORKDIR}"
    WORKDIR=""
done

if [ "${completed}" -ne "${total_iterations}" ]; then
    fail "completed ${completed} iterations, expected ${total_iterations}"
fi

# 一時ストアがディスクへ spill していれば、専用 TMPDIR に何かが残る。
LEFTOVER="$(find "${TMP_PROBE}" -mindepth 1 -maxdepth 1 | sort)"
if [ -n "${LEFTOVER}" ]; then
    COUNT="$(printf '%s\n' "${LEFTOVER}" | wc -l)"
    echo "${COUNT} temp entries were left under TMPDIR:" >&2
    printf '%s\n' "${LEFTOVER}" | head -50 >&2
    fail "crash tool left temp entries"
fi

echo "crash_test_temp_table.sh: OK (${SETS} sets x ${ITERATIONS} iterations = ${completed} SIGKILL reps)"
