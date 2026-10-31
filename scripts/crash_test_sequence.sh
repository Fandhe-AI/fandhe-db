#!/usr/bin/env bash
# Issue #1692（TABLE-24 (3)(4)・RECOVER-10・RECOVER-11・RECOVER-12）の採番状態・operation_id 台帳の
# クラッシュ耐性回帰テスト。
#
# `crates/engine/examples/crash_tool_sequence.rs`（write/verify/finish/self-check サブコマンド）を使い、
# `nextval` を含む INSERT（採番状態と台帳と行を同一 write トランザクションでコミット）を行っている
# プロセスを SIGKILL で強制終了させ、再オープン後に「行 id が連続し、全行の採番値が行 id の順に狭義
# 単調増加する」（二重採番・commit 済みの値の再払い出しが無い）ことを検証する反復を行う。
# 構成（セット × 反復回数の 2 段構成。セットごとに新規 DB を作り直す）は
# `scripts/crash_test_unique_index.sh` と同じ。追加の手順:
# - 冒頭で `self-check`（負の対照）を実行し、オラクルが不正な状態を検出できることを確かめる
# - 各セットの最後に `finish` を実行する（最後の commit の再送が台帳で拒否され、内容不一致が 22023 で
#   拒否され、どちらも値を消費しないこと）
# Issue #1700（TABLE-24・RECOVER-10〜12）の段も同じ構成で続けて実行する（`*-identity` サブコマンド。
# 従来方式の表を開き直して移行した表へ、`id` を省略した単一行・複数行・明示 txn の INSERT を
# 採番させ、「`n` の順に `id` が狭義単調増加する」ことなどを検証する。`finish-identity` は再送の
# `23505`・内容違いの `22023`・値を消費しないことを確かめる）。新しい CI ジョブは作らない。
# Issue #2229（SQL-68・RLS-10・RECOVER-11・RECOVER-12）の `DO` ブロックの段も同じ構成で続ける
# （`*-do` サブコマンド。複数表への `DO` の全か無か、明示 Tx の証人 INSERT との台帳の原子性、
# 順序 B の再送が `23505` で全体破棄になることを検証する。新しい CI ジョブは作らない）。
# Makefile の crash-test-sequence ターゲット・CI の crash-test-sequence ジョブから呼ばれる。
#
# 使い方: scripts/crash_test_sequence.sh [セット数（既定 2）] [反復回数（既定 10）]

set -u

# `kill -9` は 137、`wait` はシグナル終了したプロセスの終了コードを返すため、
# set -e はそれらと相性が悪い（意図せずスクリプトを止めてしまう）。
# 検証結果は都度 fail-closed に明示チェックする（既存の crash_test_*.sh と同方針）。

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
BIN="${REPO_ROOT}/target/release/examples/crash_tool_sequence"

echo "building crash_tool_sequence (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine --example crash_tool_sequence); then
  echo "ERROR: failed to build crash_tool_sequence" >&2
  exit 1
fi
if [ ! -x "${BIN}" ]; then
  echo "ERROR: crash_tool_sequence binary not found at ${BIN}" >&2
  exit 1
fi

# 1 モード分（self-check → セット × 反復 → finish）を実行する。引数: モード名・write・verify・finish・
# self-check の各サブコマンド名。失敗は各所で fail-closed に exit 1 する。
run_mode() {
    MODE="$1"
    WRITE_CMD="$2"
    VERIFY_CMD="$3"
    FINISH_CMD="$4"
    SELF_CMD="$5"
    echo "##### mode: ${MODE}"

    # 負の対照: オラクルが不正な状態（重複・逆順・欠落・NULL の採番値）を検出できること。
    # 空の DB に対して実行し、オラクルが空振りしないことを保証する。
    SELF_DIR="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '${SELF_DIR}'" EXIT
    self_output="$("${BIN}" "${SELF_CMD}" "${SELF_DIR}/self_check.redb")"
    echo "${self_output}"
    rm -rf "${SELF_DIR}"
    trap - EXIT
    if [[ "${self_output}" != SELF-CHECK\ ok=true* ]]; then
      echo "ERROR: self-check failed: ${self_output}" >&2
      exit 1
    fi

    # 進捗行（COMMITTED ...）が出るまでの最大待機（100ms 単位。値 300 = 30 秒）。実際に
    # 書き込みが始まっていない状態で kill しても回帰テストとして意味が無いため、
    # 開始同期をここで取る（`crash_test_unique_index.sh` と同方針）。
    WRITE_START_TIMEOUT_TENTHS=300

    for set_no in $(seq 1 "${SETS}"); do
      echo "### ${MODE} set ${set_no}/${SETS} ==="

      WORKDIR="$(mktemp -d)"
      # セットごとに新規 DB を作り直す。トラップは次セットの WORKDIR 再代入前に前セットの
      # WORKDIR を必ず片付ける。
      # shellcheck disable=SC2064
      trap "rm -rf '${WORKDIR}'" EXIT
      DB_PATH="${WORKDIR}/crash_test_sequence.redb"
      WRITE_LOG="${WORKDIR}/write.log"

      last_rows=0

      for i in $(seq 1 "${ITERATIONS}"); do
        echo "=== ${MODE} set ${set_no}/${SETS} iteration ${i}/${ITERATIONS} ==="

        : > "${WRITE_LOG}"
        "${BIN}" "${WRITE_CMD}" "${DB_PATH}" >>"${WRITE_LOG}" 2>&1 &
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
            # writer が同期点に到達する前に終了した（起動失敗など）。
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

        # コミット中に kill が当たる確率を上げるための短いランダム待機（10〜300ms。
        # `crash_tool_sequence write` は 1 行ごとに 15ms スロットリングしているため、この待機幅でも
        # 累積行数が `SCAN_LIMIT`〔10000〕に達しない）。
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
        # `wait` は SIGKILL で終了したプロセスに対して 128+9=137 を返す。それ以外は
        # SIGKILL を検証できていない反復であり、テストの目的を満たさないため拒否する。
        if [ "${writer_status}" -ne 137 ]; then
          echo "ERROR: writer pid ${writer_pid} did not terminate via SIGKILL at set ${set_no} iteration ${i} (wait exit status=${writer_status}, expected 137)" >&2
          cat "${WRITE_LOG}" >&2
          rm -rf "${WORKDIR}"
          trap - EXIT
          exit 1
        fi

        verify_output="$("${BIN}" "${VERIFY_CMD}" "${DB_PATH}")"
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
        max_n="$(echo "${verify_output}" | sed -n 's/.* max=\([0-9]*\).*/\1/p')"
        if ! [[ "${max_n}" =~ ^[0-9]+$ ]]; then
          echo "ERROR: could not parse max value from verify output: ${verify_output}" >&2
          rm -rf "${WORKDIR}"
          trap - EXIT
          exit 1
        fi

        # write.log の最後に完全に書き切られた `COMMITTED op=<id> rows=<total> n=<value>` 行から
        # 「kill 前に確認済みだった行数・採番値（acked）」を取り出す。`$` でパターンを終端に固定
        # しているため、kill が出力の途中で当たって行が切れた場合はマッチせず 1 つ前の完全な行が
        # 採用される（acked は実際より小さくなることはあっても大きくならない＝false-fail しない）。
        acked_line="$(grep -o '^COMMITTED op=[^ ]* rows=[0-9]* n=[0-9]*$' "${WRITE_LOG}" | tail -1)"
        acked="$(echo "${acked_line}" | sed -n 's/.* rows=\([0-9]*\) n=.*/\1/p')"
        acked_n="$(echo "${acked_line}" | sed -n 's/.* n=\([0-9]*\)$/\1/p')"
        if [ -n "${acked}" ]; then
          # COMMITTED は commit 成功後にのみ出力されるため、再オープン後の rows・max はその時点で
          # 確認済みだった値を下回ってはならない（下回れば、確認済みのはずのコミットが失われたことに
          # なる＝data loss）。
          if [ "${rows}" -lt "${acked}" ]; then
            echo "ERROR: row count ${rows} is below the last acknowledged commit (rows=${acked} from write.log), data loss suspected (set ${set_no} iteration ${i})" >&2
            rm -rf "${WORKDIR}"
            trap - EXIT
            exit 1
          fi
          if [ -n "${acked_n}" ] && [ "${max_n}" -lt "${acked_n}" ]; then
            echo "ERROR: max sequence value ${max_n} is below the last acknowledged value (n=${acked_n} from write.log), data loss suspected (set ${set_no} iteration ${i})" >&2
            rm -rf "${WORKDIR}"
            trap - EXIT
            exit 1
          fi
        fi

        # "started" が true な時点で write は kill 前に少なくとも 1 回 COMMITTED を出力済みなので、
        # rows は必ず last_rows より真に増えているはずである。
        if [ "${rows}" -le "${last_rows}" ]; then
          echo "ERROR: row count did not increase across restarts (${last_rows} -> ${rows}) despite a committed row, data loss suspected (set ${set_no} iteration ${i})" >&2
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

      # セットの最後に 1 回: 最後の commit の再送が台帳で拒否され、内容不一致が拒否され、どちらも値を
      # 消費しないこと（副作用あり。次のセットは新規 DB から始まる）。
      finish_output="$("${BIN}" "${FINISH_CMD}" "${DB_PATH}")"
      echo "${finish_output}"
      if [[ "${finish_output}" != RESULT\ ok=true* ]]; then
        echo "ERROR: finish failed at set ${set_no}: ${finish_output}" >&2
        rm -rf "${WORKDIR}"
        trap - EXIT
        exit 1
      fi

      rm -rf "${WORKDIR}"
      trap - EXIT
    done
}

# 進捗の合計（3 モード分）。各モードが `completed` を加算する。
total_iterations=$((3 * SETS * ITERATIONS))
completed=0

run_mode sequence write verify finish self-check
run_mode identity write-identity verify-identity finish-identity self-check-identity
run_mode do write-do verify-do finish-do self-check-do

if [ "${completed}" -ne "${total_iterations}" ]; then
  echo "ERROR: completed ${completed} iterations, expected ${total_iterations}" >&2
  exit 1
fi

echo "crash_test_sequence.sh: OK (3 modes x ${SETS} sets x ${ITERATIONS} iterations = ${completed} SIGKILL reps)"
