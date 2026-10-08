#!/usr/bin/env bash
# TASK-142（対象ビヘイビア: PERSIST-1。ポインタ: docs/spec/05-tasks.md TASK-142）の
# クラッシュ耐性回帰テスト。
#
# `crates/engine/examples/crash_tool.rs`（write/verify サブコマンド）を使い、
# 書き込み進行中の engine::storage::Storage プロセスを SIGKILL で強制終了させ、
# 再オープン後の内容整合性を検証する反復を行う。プロセス内テスト
# （crates/engine/tests/persistence.rs）では表現できない「プロセス外からの
# 強制終了」を成立させるための補助スクリプトで、Makefile の crash-test
# ターゲット・CI の crash-test ジョブから呼ばれる。
#
# 使い方: scripts/crash_test.sh [反復回数（既定 10）]

set -u

# `kill -9` は 137、`wait` はシグナル終了したプロセスの終了コードを返すため、
# set -e はそれらと相性が悪い（意図せずスクリプトを止めてしまう）。
# 検証結果は都度 fail-closed に明示チェックする。

ITERATIONS="${1:-10}"
if ! [[ "${ITERATIONS}" =~ ^[0-9]+$ ]] || [ "${ITERATIONS}" -lt 1 ]; then
  echo "ERROR: iterations must be a positive integer, got: ${ITERATIONS}" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${REPO_ROOT}/target/release/examples/crash_tool"

echo "building crash_tool (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-engine --example crash_tool); then
  echo "ERROR: failed to build crash_tool" >&2
  exit 1
fi
if [ ! -x "${BIN}" ]; then
  echo "ERROR: crash_tool binary not found at ${BIN}" >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
if [ ! -d "${WORKDIR}" ]; then
  echo "ERROR: mktemp -d failed to create a working directory" >&2
  exit 1
fi

# writer（crash_tool write）の PID を保持する。ループ内で起動するたびに更新され、
# `wait` で reap 済みなら空文字へ戻す（不変条件は stop_writer のコメント参照）。
# trap 設定（後述の trap cleanup EXIT / INT TERM）より前に初期化しておかないと、
# trap 設置後・ループ内で writer_pid=$! に到達する前の窓で SIGTERM/SIGINT を受けた
# 場合に set -u 下で cleanup が未定義変数エラーになり、
# 本来の目的（writer 停止）を果たさず異常終了してしまう。
writer_pid=""

# バックグラウンドの writer（crash_tool write）を確実に停止する。
# 呼び出し元は cleanup（EXIT/INT/TERM 経路）とループ内の各停止経路。
# - writer_pid が空なら何もしない（reap 済み PID は OS に再利用され得るため、
#   空のまま kill すると無関係な別プロセスを誤って殺しかねない）
# - SIGKILL を使うのは、writer 側にシグナルハンドラが無く WORKDIR ごと破棄される
#   前提のプロセスであり、graceful 停止に意味が無い・ハングしない・本スクリプトの
#   他の停止経路（成功パスの SIGKILL 判定）と手段を揃えられるため
# - wait で確実に reap してから writer_pid をクリアする（クリア漏れがあると
#   cleanup からの再呼び出しで別プロセスへの誤 kill につながる）
stop_writer() {
  if [ -n "${writer_pid}" ]; then
    kill -9 "${writer_pid}" 2>/dev/null
    wait "${writer_pid}" 2>/dev/null
    writer_pid=""
  fi
}

# INT/TERM は bash の trap ハンドラ実行後もスクリプトの次の文から実行を継続する
# 仕様があるため、EXIT と INT/TERM を同一 trap にまとめると中断時に WORKDIR 削除後も
# ループが続行し、以降の参照が不可解なエラーで異常終了する。cleanup（削除処理）は
# EXIT 用に分離し、INT/TERM では明示的に exit 130 で打ち切ることでハンドラ実行後の
# 継続を防ぐ（130 は SIGINT の慣例終了コードだが、本スクリプトでは TERM 受信時も
# 同じ値で統一して打ち切る）。
#
# cleanup は「writer 停止 → WORKDIR 削除」の順序を守る（本質的な点）。逆順にすると
# WORKDIR を unlink した後も writer が生存を続け、削除済み redb ファイルへ書き込みを
# 続ける survivor が残る（プロセスグループではなくスクリプト単体へ SIGTERM/SIGINT が
# 送られた場合、trap されるのは本スクリプトのみで子である writer には届かないため、
# 明示的な停止が無いと writer は親を失ったまま MAX_ROWS まで自走しディスク・CPU を
# 消費し続ける。Issue #134）。
# `cleanup; exit 130` は EXIT trap も再度走らせる（exit で EXIT trap が発火する）が、
# stop_writer は writer_pid 空なら no-op・`rm -rf` は対象が既に無ければ何もしないため
# 2 回実行しても安全（冪等）。
cleanup() {
  stop_writer
  rm -rf "${WORKDIR}"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

# 中断パスの検証手順（受け入れ基準: Issue #134）。自動化版は
# scripts/crash_test_interrupt.sh（`make crash-test-interrupt`）。
#   - スクリプト単体への SIGTERM（プロセスグループを介さない経路）:
#     scripts/crash_test.sh 100 & pid=$!; sleep 5; kill -TERM "$pid"; wait "$pid"
#     → 終了コード 130・`pgrep -f 'crash_tool write'` の出力が空（survivor 無し）
#   - プロセスグループへの SIGTERM（`set -m` 下・端末の Ctrl+C 相当）:
#     ( set -m; scripts/crash_test.sh 100 & pid=$!; sleep 5; kill -TERM -- "-$pid"; wait "$pid" )
#     → 同様に survivor 無しであることを確認する

# 以下の 1 行は単なる進捗表示ではなく、scripts/crash_test_interrupt.sh が
# `sed -n 's/^workdir: //p'` で WORKDIR を取り出し「中断後に削除されたか」を
# 判定するための契約（load-bearing）。書式（行頭 `workdir: ` + パス）を変更する
# 場合は crash_test_interrupt.sh のパースも併せて更新すること。
echo "workdir: ${WORKDIR}"
DB_PATH="${WORKDIR}/crash_test.redb"
WRITE_LOG="${WORKDIR}/write.log"

# 進捗行（COMMITTED ...）が出るまでの最大待機（100ms 単位。値 300 = 30 秒）。実際に
# 書き込みが始まっていない状態で kill しても回帰テストとして意味が無いため、
# 開始同期をここで取る（コールド runner での初回 put 遅延を見込んだ余裕）。
WRITE_START_TIMEOUT_TENTHS=300

last_rows=0

for i in $(seq 1 "${ITERATIONS}"); do
  echo "=== iteration ${i}/${ITERATIONS} ==="

  : > "${WRITE_LOG}"
  "${BIN}" write "${DB_PATH}" >> "${WRITE_LOG}" 2>&1 &
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
      # kill -0 は zombie に対しては成功するため、失敗＝ESRCH＝bash が既に reap
      # 済みで PID が OS に再利用され得る状態。writer_pid の「未 reap の子だけを
      # 指す」不変条件を保つためクリアし、下の stop_writer が再利用済み PID へ
      # kill -9 を送らないようにする（タイムアウトで抜ける経路では writer は生存中
      # なので writer_pid は保持され、stop_writer が正しく停止する）。
      writer_pid=""
      break
    fi
    sleep 0.1
    waited=$((waited + 1))
  done

  if [ "${started}" != "true" ]; then
    timeout_ms=$((WRITE_START_TIMEOUT_TENTHS * 100))
    echo "ERROR: writer did not report progress within ${timeout_ms}ms" >&2
    stop_writer
    exit 1
  fi

  # コミット中に kill が当たる確率を上げるための短いランダム待機（10〜300ms）。
  wait_ms=$((10 + RANDOM % 291))
  sleep "0.$(printf '%03d' "${wait_ms}")"

  if ! kill -9 "${writer_pid}" 2>/dev/null; then
    # kill が失敗する＝対象 PID が既に存在しない（ESRCH）。writer は自発終了し
    # bash に reap 済みで、その PID は OS に再利用され得る。ここで writer_pid を
    # 保持したまま exit 1 すると EXIT trap（cleanup → stop_writer）が同じ PID へ
    # 再度 kill -9 を送り、無関係なプロセスを巻き込む。未 reap の子だけを指す
    # という writer_pid の不変条件を保つため、退避してから即クリアする。
    reaped_writer_pid="${writer_pid}"
    writer_pid=""
    echo "ERROR: kill -9 failed for writer pid ${reaped_writer_pid} at iteration ${i} (writer may have already exited on its own, e.g. a put error or safety limit); this iteration cannot verify SIGKILL crash resilience" >&2
    cat "${WRITE_LOG}" >&2
    exit 1
  fi

  wait "${writer_pid}" 2>/dev/null
  writer_status=$?
  # reap 済みの PID は OS に回収され別プロセスへ再利用され得る。共有変数
  # writer_pid は「未 reap の子だけを指す」不変条件（stop_writer のコメント参照）
  # を保つため、終了状態の判定より前にクリアする。判定を挟んでからクリアすると、
  # 下の fail-closed な `exit 1` 経路が writer_pid を保持したまま EXIT trap
  # （cleanup → stop_writer）へ入り、reap 済み PID へ kill -9 を送って同一 runner 上の
  # 無関係なプロセスを巻き込みかねない。メッセージ用には退避した値を使う。
  reaped_writer_pid="${writer_pid}"
  writer_pid=""

  # `wait` は SIGKILL で終了したプロセスに対して 128+9=137 を返す。
  # それ以外（writer が kill 到達前に自発終了した等）は SIGKILL を検証できて
  # いない反復であり、テストの目的（強制終了下での整合性検証）を満たさない
  # ため fail-closed で拒否する。
  if [ "${writer_status}" -ne 137 ]; then
    echo "ERROR: writer pid ${reaped_writer_pid} did not terminate via SIGKILL at iteration ${i} (wait exit status=${writer_status}, expected 137); this iteration did not exercise a real SIGKILL crash" >&2
    cat "${WRITE_LOG}" >&2
    exit 1
  fi

  verify_output="$("${BIN}" verify "${DB_PATH}")"
  echo "${verify_output}"

  if [[ "${verify_output}" != RESULT\ ok=true* ]]; then
    echo "ERROR: verify failed at iteration ${i}: ${verify_output}" >&2
    exit 1
  fi

  rows="$(echo "${verify_output}" | sed -n 's/.*rows=\([0-9]*\).*/\1/p')"
  if ! [[ "${rows}" =~ ^[0-9]+$ ]]; then
    echo "ERROR: could not parse row count from verify output: ${verify_output}" >&2
    exit 1
  fi

  # write.log の最後に完全に書き切られた `COMMITTED row=<n> rows=<total>` 行から
  # 「kill 前に確認済みだった行数（acked）」を取り出す。`$` でパターンを終端に
  # 固定しているため、kill が出力の途中で当たって行が切れた場合はマッチせず
  # 1 つ前の完全な行が採用される（acked は実際より小さくなることはあっても
  # 大きくなることはない＝false-fail しない）。
  acked="$(grep -o '^COMMITTED row=[0-9]* rows=[0-9]*$' "${WRITE_LOG}" | tail -1 | sed -n 's/.*rows=\([0-9]*\)$/\1/p')"
  if [ -n "${acked}" ]; then
    # 「started」で確認した COMMITTED はコミット成功後にのみ出力されるため、
    # 再オープン後の rows はその時点で確認済みだった acked 行数を下回ってはならない
    # （下回れば、確認済みのはずのコミットが失われたことになる＝data loss）。
    if [ "${rows}" -lt "${acked}" ]; then
      echo "ERROR: row count ${rows} is below the last acknowledged commit (rows=${acked} from write.log), data loss suspected" >&2
      exit 1
    fi
  fi

  # "started" が true な時点で write は kill 前に少なくとも 1 回 COMMITTED を
  # 出力済み（＝コミットが実際に成功した後にのみ出力される同期点を通過済み）
  # なので、rows は必ず last_rows より真に増えているはずである。`-lt` のみだと
  # 新規コミットが全て失われても rows == last_rows で見逃してしまう
  # （data loss を検出できない）ため、`-le` で「増えていない」ケースも失敗にする。
  if [ "${rows}" -le "${last_rows}" ]; then
    echo "ERROR: row count did not increase across restarts (${last_rows} -> ${rows}) despite a committed write, data loss suspected" >&2
    exit 1
  fi
  last_rows="${rows}"
done

# 空虚な成功（DB 未作成・0 行のまま検証をすり抜ける）を拒否する。
if [ "${last_rows}" -le 0 ]; then
  echo "ERROR: final row count is not positive (${last_rows})" >&2
  exit 1
fi

echo "crash_test.sh: OK (${ITERATIONS} iterations, final rows=${last_rows})"
