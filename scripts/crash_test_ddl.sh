#!/usr/bin/env bash
# Issue #2417（親 TASK-332。対象ビヘイビア: PERSIST-1・TABLE-10。ポインタ: docs/spec/04-behavior/）の
# 「1 文の DDL の実行」クラッシュ耐性回帰テスト。
#
# `crates/engine/examples/crash_tool_ddl.rs`（write/verify/self-check サブコマンド）を使い、
# `CREATE TABLE`・`INSERT`・`ALTER TABLE ADD COLUMN`・スカラー／一意の `CREATE INDEX`・
# `DROP TABLE` を 1 文ずつ実行中のプロセスを SIGKILL で強制終了させ、再オープン後の
# カタログ・行・索引宣言・物理表が「文の前」か「文の後」のどちらか一方に完全一致すること
# （部分的な DDL が 0 件）を検証する反復を行う。骨格は `scripts/crash_test_cross_table.sh` と同じ
# （セット × 反復の 2 段構成。セットごとに新規 DB）。冒頭で `self-check`（オラクルの空振り防止の
# 負の対照）を実行し、失敗すれば反復へ進まない。Makefile の crash-test-ddl ターゲット・CI の
# crash-test-cross-table ジョブの 1 step から呼ばれる。DDL の途中のエラーで何も残らないことの
# 検査は #2419 が担当する。
#
# Issue #2418（SQL-23 を併せて参照）: write.log の最後の進行行（STMT / COMMITTED）で kill 位置を
# 分類する。STMT なら文の実行中で、再起動後の n は ack 済み n（文の前）か +1（文の後）のどちらか。
# COMMITTED なら文と文の間で、n は ack 済み n と完全一致（幽霊適用の検出）。DDL 段（stage 0/2/3/4/5）
# の実行中の kill を run 全体で 1 度も観測しなければ失敗にする（空虚な成功の拒否）。判定単位を
# 反復ごとにしないのは遅い runner での不安定化を避けるため（アサーションの弱体化ではない）。
#
# 使い方: scripts/crash_test_ddl.sh [セット数（既定 1）] [反復回数（既定 60）]

set -u

# `kill -9` は 137、`wait` はシグナル終了したプロセスの終了コードを返すため、
# set -e はそれらと相性が悪い（意図せずスクリプトを止めてしまう）。
# 検証結果は都度 fail-closed に明示チェックする。

SETS="${1:-1}"
ITERATIONS="${2:-60}"
if ! [[ "${SETS}" =~ ^[0-9]+$ ]] || [ "${SETS}" -lt 1 ]; then
  echo "ERROR: sets must be a positive integer, got: ${SETS}" >&2
  exit 1
fi
if ! [[ "${ITERATIONS}" =~ ^[0-9]+$ ]] || [ "${ITERATIONS}" -lt 1 ]; then
  echo "ERROR: iterations must be a positive integer, got: ${ITERATIONS}" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${REPO_ROOT}/target/release/examples/crash_tool_ddl"

echo "building crash_tool_ddl (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine --example crash_tool_ddl); then
  echo "ERROR: failed to build crash_tool_ddl" >&2
  exit 1
fi
if [ ! -x "${BIN}" ]; then
  echo "ERROR: crash_tool_ddl binary not found at ${BIN}" >&2
  exit 1
fi

# オラクルの空振り防止（正例の全状態受理・負例の拒否）。失敗なら反復へ進まない。
SELF_DIR="$(mktemp -d)"
if ! "${BIN}" self-check "${SELF_DIR}"; then
  echo "ERROR: self-check failed" >&2
  rm -rf "${SELF_DIR}"
  exit 1
fi
rm -rf "${SELF_DIR}"

# 進捗行（COMMITTED ...）が出るまでの最大待機（100ms 単位。値 300 = 30 秒）。実際に
# 書き込みが始まっていない状態で kill しても回帰テストとして意味が無いため、
# 開始同期をここで取る（コールド runner での初回コミット遅延を見込んだ余裕）。
WRITE_START_TIMEOUT_TENTHS=300

total_iterations=$((SETS * ITERATIONS))
completed=0
# kill 位置の観測数（run 全体）。before/after は実行中の kill の結果（文の前/後）、by_stage は段別
# （0 CREATE TABLE / 1 INSERT / 2 ALTER / 3 CREATE INDEX / 4 CREATE UNIQUE INDEX / 5 DROP TABLE）。
inflight_before=0
inflight_after=0
ddl_inflight=0
declare -a inflight_by_stage=(0 0 0 0 0 0)

for set_no in $(seq 1 "${SETS}"); do
  echo "### set ${set_no}/${SETS} ==="

  WORKDIR="$(mktemp -d)"
  # セットごとに新規 DB を作り直す。set のトラップは
  # このセットのループを抜けるたびに実行され、次セットの WORKDIR 再代入前に
  # 前セットの WORKDIR を必ず片付ける。
  # shellcheck disable=SC2064
  trap "rm -rf '${WORKDIR}'" EXIT
  DB_PATH="${WORKDIR}/crash_test_ddl.redb"
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

    # コミット中に kill が当たる確率を上げるための短いランダム待機（10〜300ms）。
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

    # write.log の最後に完全に書き切られた進行行（STMT=文の実行開始 / COMMITTED=ack）で kill 位置を
    # 分類する。`$` で終端を固定しているため、出力の途中で切れた行は無視され 1 つ前の完全な行が採用される。
    # started 同期で COMMITTED を 1 つは見ているので、acked（= ack 済みの段数 n）は必ず取れる。
    last_line="$(grep -E '^(STMT step=[0-9]+ stage=[0-9]+|COMMITTED batch=[0-9]+ rows=[0-9]+)$' "${WRITE_LOG}" | tail -1)"
    acked="$(grep -o '^COMMITTED batch=[0-9]* rows=[0-9]*$' "${WRITE_LOG}" | tail -1 | sed -n 's/.*rows=\([0-9]*\)$/\1/p')"
    if ! [[ "${acked}" =~ ^[0-9]+$ ]] || [ -z "${last_line}" ]; then
      echo "ERROR: could not classify the kill position from write.log (set ${set_no} iteration ${i})" >&2
      cat "${WRITE_LOG}" >&2
      rm -rf "${WORKDIR}"
      trap - EXIT
      exit 1
    fi
    if [[ "${last_line}" =~ ^STMT\ step=([0-9]+)\ stage=([0-9]+)$ ]]; then
      # 文の実行中。直前の ack 済み n と開始した step が一致するはず（ログの整合）。
      stmt_step="${BASH_REMATCH[1]}"
      stmt_stage="${BASH_REMATCH[2]}"
      if [ "${stmt_step}" -ne "${acked}" ]; then
        echo "ERROR: in-flight step ${stmt_step} does not follow acked ${acked} (set ${set_no} iteration ${i})" >&2
        rm -rf "${WORKDIR}"
        trap - EXIT
        exit 1
      fi
      if [ "${rows}" -eq "${acked}" ]; then
        inflight_before=$((inflight_before + 1))
      elif [ "${rows}" -eq $((acked + 1)) ]; then
        inflight_after=$((inflight_after + 1))
      else
        echo "ERROR: n=${rows} is neither before (${acked}) nor after ($((acked + 1))) the in-flight statement (set ${set_no} iteration ${i})" >&2
        rm -rf "${WORKDIR}"
        trap - EXIT
        exit 1
      fi
      inflight_by_stage[stmt_stage]=$((${inflight_by_stage[stmt_stage]:-0} + 1))
      if [ "${stmt_stage}" -ne 1 ]; then
        ddl_inflight=$((ddl_inflight + 1))
      fi
    else
      # 文と文の間。ack を超える幽霊適用も、ack 済みの消失も許さない（完全一致）。
      if [ "${rows}" -ne "${acked}" ]; then
        echo "ERROR: n=${rows} differs from acked ${acked} although no statement was in flight (set ${set_no} iteration ${i})" >&2
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

# DDL 段（INSERT 以外）の実行中の中断を 1 度も観測していない実行は、DDL の途中を検証できていない。
if [ "${ddl_inflight}" -le 0 ]; then
  echo "ERROR: no kill landed while a DDL statement was in flight in ${completed} reps (before=${inflight_before} after=${inflight_after}); raise iterations" >&2
  exit 1
fi

echo "crash_test_ddl.sh: OK (${SETS} sets x ${ITERATIONS} iterations = ${completed} SIGKILL reps; in-flight kills ddl=${ddl_inflight} before=${inflight_before} after=${inflight_after} by_stage=${inflight_by_stage[*]})"
