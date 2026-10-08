#!/usr/bin/env bash
# Issue #1070（TABLE-16・TASK-204）の永続一意索引・クラッシュ耐性回帰テスト。
#
# `crates/engine/examples/crash_tool_unique_index.rs`（write/verify サブコマンド）を使い、
# `docs (code TEXT UNIQUE)` への INSERT（＝行ストア `user_rows/{table}` と永続一意索引
# `user_uniq/{table}` を同一 write トランザクションでコミット）を行っているプロセスを
# SIGKILL で強制終了させ、再オープン後に索引と行データが整合していることを検証する
# 反復を行う。2 テーブル横断トランザクションを検証する `scripts/crash_test_cross_table.sh`
# （TASK-90）と構成（セット × 反復回数の 2 段構成。セットごとに新規 DB を作り直す）は
# 同じだが、あちらは `ROWS_TABLE`（SQL 表層が触れない旧テーブル）しか書かないため
# UNIQUE 制約の検証には使えない（本スクリプトのツール自身のドキュメント参照）。
# Makefile の crash-test-unique-index ターゲット・CI の crash-test-unique-index
# ジョブから呼ばれる。
#
# 使い方: scripts/crash_test_unique_index.sh [セット数（既定 2）] [反復回数（既定 10）]

set -u

# `kill -9` は 137、`wait` はシグナル終了したプロセスの終了コードを返すため、
# set -e はそれらと相性が悪い（意図せずスクリプトを止めてしまう）。
# 検証結果は都度 fail-closed に明示チェックする。

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
BIN="${REPO_ROOT}/target/release/examples/crash_tool_unique_index"

echo "building crash_tool_unique_index (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine --example crash_tool_unique_index); then
  echo "ERROR: failed to build crash_tool_unique_index" >&2
  exit 1
fi
if [ ! -x "${BIN}" ]; then
  echo "ERROR: crash_tool_unique_index binary not found at ${BIN}" >&2
  exit 1
fi

# 進捗行（COMMITTED ...）が出るまでの最大待機（100ms 単位。値 300 = 30 秒）。実際に
# 書き込みが始まっていない状態で kill しても回帰テストとして意味が無いため、
# 開始同期をここで取る（コールド runner での初回コミット遅延を見込んだ余裕。
# `crash_test_cross_table.sh` と同方針）。
WRITE_START_TIMEOUT_TENTHS=300

total_iterations=$((SETS * ITERATIONS))
completed=0

for set_no in $(seq 1 "${SETS}"); do
  echo "### set ${set_no}/${SETS} ==="

  WORKDIR="$(mktemp -d)"
  # セットごとに新規 DB を作り直す。set のトラップはこのセットのループを抜ける
  # たびに実行され、次セットの WORKDIR 再代入前に前セットの WORKDIR を必ず片付ける
  # （`crash_test_cross_table.sh` と同方針）。
  # shellcheck disable=SC2064
  trap "rm -rf '${WORKDIR}'" EXIT
  DB_PATH="${WORKDIR}/crash_test_unique_index.redb"
  WRITE_LOG="${WORKDIR}/write.log"

  last_rows=0

  for i in $(seq 1 "${ITERATIONS}"); do
    echo "=== set ${set_no}/${SETS} iteration ${i}/${ITERATIONS} ==="

    : > "${WRITE_LOG}"
    "${BIN}" write "${DB_PATH}" >>"${WRITE_LOG}" 2>&1 &
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
      rm -rf "${WORKDIR}"
      trap - EXIT
      exit 1
    fi

    # コミット中に kill が当たる確率を上げるための短いランダム待機（10〜300ms。
    # `crash_tool_unique_index write` は 1 バッチごとに 20ms スロットリングして
    # いるため、この待機幅でも累積行数が `SCAN_LIMIT`〔10000〕に達しない）。
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

    verify_output="$("${BIN}" verify "${DB_PATH}")"
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

    # write.log の最後に完全に書き切られた `COMMITTED batch=<seq> rows=<total>` 行から
    # 「kill 前に確認済みだった行数（acked）」を取り出す。`$` でパターンを終端に固定
    # しているため、kill が出力の途中で当たって行が切れた場合はマッチせず 1 つ前の
    # 完全な行が採用される（acked は実際より小さくなることはあっても大きくならない
    # ＝false-fail しない。`crash_test_cross_table.sh` と同方針）。
    acked="$(grep -o '^COMMITTED batch=[0-9]* rows=[0-9]*$' "${WRITE_LOG}" | tail -1 | sed -n 's/.*rows=\([0-9]*\)$/\1/p')"
    if [ -n "${acked}" ]; then
      # 「started」で確認した COMMITTED はコミット成功後にのみ出力されるため、
      # 再オープン後の rows はその時点で確認済みだった acked 行数を下回ってはならない
      # （下回れば、確認済みのはずのコミットが失われたことになる＝data loss）。
      if [ "${rows}" -lt "${acked}" ]; then
        echo "ERROR: row count ${rows} is below the last acknowledged commit (rows=${acked} from write.log), data loss suspected (set ${set_no} iteration ${i})" >&2
        rm -rf "${WORKDIR}"
        trap - EXIT
        exit 1
      fi
    fi

    # "started" が true な時点で write は kill 前に少なくとも 1 回 COMMITTED を
    # 出力済み（＝1 バッチのコミットが実際に成功した後にのみ出力される同期点を通過済み）
    # なので、rows は必ず last_rows より真に増えているはずである。
    if [ "${rows}" -le "${last_rows}" ]; then
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

echo "crash_test_unique_index.sh: OK (${SETS} sets x ${ITERATIONS} iterations = ${completed} SIGKILL reps)"
