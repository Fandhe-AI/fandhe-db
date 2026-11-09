#!/usr/bin/env bash
# Issue #2170（RECOVER-14 ポインタ。親 #2164）の変更ログ・PITR の crash 耐性／ラウンドトリップ回帰テスト。
#
# 変更ログ（`--change-log`）とアーカイブ（`--change-log-archive-dir`）を有効にしたサーバーを書き込みの
# 最中に SIGKILL し、フルバックアップ（ベース）とアーカイブから `restore --until-lsn|--until-time` で
# 復元した状態が、クラッシュ直前に実際にコミットされていた状態と一致することを確かめる。あわせて
# ログの欠落・改ざん・並べ替えを `change-log-verify` と PITR の `restore` の両方が拒否し、何も公開しない
# ことを確かめる。#2157 の `crash_test_backup_restore.sh` と同じ流儀（運用者が使う CLI の経路そのものを
# リリースバイナリで対象にし、最小 pg wire クライアント crash_test_backup_restore_client.py〔標準ライブラリ
# のみ〕を再利用する）。
#
# 前提となる性質: 追記クライアントの 1 回の INSERT はちょうど 1 回の commit（LSN が 1 進む）で、行 id は
# 連番にする。これにより「LSN → 件数・最大 id」の対応が決まり、検査点どうしの差で各点の整合を確かめる。
#
# 手順:
#   1. 種データを投入し、アーカイブが追いついた時点（検査点 0）でベース（フルバックアップ）を取る。
#      検査点 = (LSN, 件数, 最大 id, 時刻)。LSN は、サーバーを再起動して（修復と起動直後のアーカイブ）
#      アーカイブの末尾 LSN が「前の検査点の LSN + 件数の増分」に一致するまで待って採る。
#   2. クラッシュ周期: サーバーを起動して追記し、追記の最中に SIGKILL する。周期ごとに検査点を採り、
#      確認済み（commit 応答を受けた）最大 id がすべて含まれること、commit 済みで未確認の行が高々 1 行で
#      あること、LSN と件数・id の増分が 1 対 1 であることを確かめる。
#   3. 停止後の `change-log-verify`（アーカイブ単体と、DB のコピーとの突き合わせ）が終了コード 0。
#   4. ベース + アーカイブから各検査点の LSN・各周期の中間 LSN・検査点の時刻まで復元し、`verify` が 0 で、
#      サーバーで読んだ件数・最大 id が期待値と一致すること（半端な commit が無いこと）。範囲外の指定は
#      終了コード 1 で拒否され何も公開されないこと。
#   5. PITR の `restore` の最中に SIGKILL する反復。最終名は「存在しない」か「完全」のどちらかで、残骸は
#      一時名（`.fandhe-restore-*.tmp`）だけ、入力（ベース・アーカイブ）は変わらないこと。
#   6. 改ざん: アーカイブの複製に対し、途中のセグメントの削除・1 バイト反転・並べ替え、停止点より後ろの
#      セグメントの反転・切り詰め、末尾セグメントの削除を行い、`change-log-verify`（末尾削除は DB のコピーとの
#      突き合わせで拒否）も `restore --until-lsn` も終了コード 1 で拒否し、復元先に何も作らないこと（無改ざんの対照は成功する）。
#   7. 増分併用: 途中の検査点の DB から取った増分バックアップ + アーカイブで最新まで復元できること、
#      増分だけで増分内の LSN まで復元できること、増分の反転を拒否すること。
# 時刻指定の検査は壁時計に依存する。開発機では別ジョブの影響で壁時計が数百 ms 単位で前後に飛ぶことが
# あったため、検査点の前後で壁時計が単調時計（/proc/uptime）から 30ms を超えてずれた場合は、飛びの無い
# 検査点が採れるまでクラッシュ周期を足す（上限あり。採れなければ失敗）。
# 空虚な成功を避けるため、書き込み中にサーバーを kill できた周期数と、PITR の restore を公開前
# （一時名が新たにあり最終名が無い）に kill できた回数を数え、0 回なら失敗とする（反復を延長する。上限あり）。
# Makefile の crash-test-pitr ターゲット（crash-test の前提）から呼ばれ、CI では既存の crash-test
# ジョブで実行される。
#
# 使い方: scripts/crash_test_pitr.sh [restore 中 kill の反復回数（既定 8）] [クラッシュ周期数（既定 4）]

set -u

# `kill -9` は 137、`wait` はシグナル終了したプロセスの終了コードを返すため、set -e は
# 相性が悪い。検証結果は都度 fail-closed に明示チェックする（crash_test.sh と同方針）。

ITERATIONS="${1:-8}"
CYCLES="${2:-4}"
for v in "${ITERATIONS}" "${CYCLES}"; do
  if ! [[ "${v}" =~ ^[0-9]+$ ]] || [ "${v}" -lt 1 ]; then
    echo "ERROR: arguments must be positive integers, got: ${v}" >&2
    exit 1
  fi
done
if [ "${CYCLES}" -lt 3 ]; then
  echo "ERROR: at least 3 crash cycles are required (tamper cases need 4+ archive segments)" >&2
  exit 1
fi

# 時刻指定の検査が壁時計と単調時計（/proc/uptime）の差を使うため Linux 専用。bash 5 未満は $EPOCHREALTIME が無い。
[ -r /proc/uptime ] || { echo "ERROR: Linux (/proc/uptime) is required" >&2; exit 1; }
if [ "${BASH_VERSINFO[0]}" -lt 5 ]; then
  echo "ERROR: bash 5 or later is required (EPOCHREALTIME)" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${REPO_ROOT}/target/release/wire-server"
CLIENT="${REPO_ROOT}/scripts/crash_test_backup_restore_client.py"

if ! command -v python3 >/dev/null 2>&1; then
  echo "ERROR: python3 is required (pg wire client helper)" >&2
  exit 1
fi

echo "building wire-server (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-wire-server --bin wire-server); then
  echo "ERROR: failed to build wire-server" >&2
  exit 1
fi
if [ ! -x "${BIN}" ]; then
  echo "ERROR: wire-server binary not found at ${BIN}" >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
if [ ! -d "${WORKDIR}" ]; then
  echo "ERROR: mktemp -d failed to create a working directory" >&2
  exit 1
fi

# バックグラウンドの子（サーバー・追記クライアント・restore の CLI）の PID。trap 設置前に初期化する
# （set -u 下で cleanup が未定義参照しない）。
server_pid=""
client_pid=""
job_pid=""

# 子を確実に停止し reap する。空 PID は何もしない（reap 済み PID の再利用による誤 kill 防止）。
stop_pid() {
  local pid="$1"
  if [ -n "${pid}" ]; then
    kill -9 "${pid}" 2>/dev/null
    wait "${pid}" 2>/dev/null
  fi
}

# cleanup は「子の停止 → WORKDIR 削除」の順（逆順だと削除済みファイルへ書く子が残る）。
# INT/TERM は bash の仕様でハンドラ後も実行が継続するため、EXIT と分離して exit 130 で打ち切る。
cleanup() {
  stop_pid "${job_pid}"
  job_pid=""
  stop_pid "${client_pid}"
  client_pid=""
  stop_pid "${server_pid}"
  server_pid=""
  rm -rf "${WORKDIR}"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

# crash_test_interrupt.sh と同じ形式（行頭 `workdir: ` + パス）。
echo "workdir: ${WORKDIR}"

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

SRC_DIR="${WORKDIR}/src"       # 元 DB・users・サーバーログ
ARCH="${WORKDIR}/arch"         # 変更ログのアーカイブ先（0700 が必須）
BAK_DIR="${WORKDIR}/bak"       # ベース・増分・DB のコピー
RST_DIR="${WORKDIR}/rst"       # 検査用の復元先
RSTK_DIR="${WORKDIR}/rstk"     # restore 中 kill の復元先（残骸検査の対象）
TAMP_DIR="${WORKDIR}/tamp"     # 改ざん用のアーカイブ複製・その復元先
mkdir "${SRC_DIR}" "${ARCH}" "${BAK_DIR}" "${RST_DIR}" "${RSTK_DIR}" "${TAMP_DIR}"
chmod 700 "${ARCH}" "${BAK_DIR}" "${TAMP_DIR}"

USER_NAME="alice"
USER_PASS="pw-crash-test"
USERS_FILE="${SRC_DIR}/users.txt"
SERVER_LOG="${SRC_DIR}/server.log"
CLIENT_LOG="${SRC_DIR}/client.log"
SRC_DB="${SRC_DIR}/source.redb"

# 初期ロール用のユーザーストア（ダミーのテスト用パスワード。argon2id の PHC 文字列を生成する）。
phc="$(printf '%s\n' "${USER_PASS}" | "${BIN}" hash-password)" || fail "hash-password failed"
printf '%s:tenant-a:%s\n' "${USER_NAME}" "${phc}" > "${USERS_FILE}"

# 種データの行数（1 行約 1KB）。
SEED_ROWS=3000
# サーバー起動待ち・アーカイブ待ちの上限（100ms 単位。300 = 30 秒）。
START_TIMEOUT_TENTHS=300
# アーカイブ周期（秒）。短くして検査点の採取待ちを短縮する。
ARCHIVE_INTERVAL_SECS=1

sleep_ms() {
  local ms="$1"
  sleep "$(printf '%d.%03d' $((ms / 1000)) $((ms % 1000)))"
}

now_ms() {
  echo $(($(date +%s%N) / 1000000))
}

# サーバーを起動する。引数は `<db パス> [追加オプション...]`。変更ログは常に有効（一度有効にした DB は
# フラグなしでは起動できないため）。成功すると server_pid と SERVER_PORT が設定される。
SERVER_PORT=""
start_server() {
  local db="$1"
  shift
  : > "${SERVER_LOG}"
  "${BIN}" --users "${USERS_FILE}" --db "${db}" --bind 127.0.0.1:0 \
    --ddl-allowed-users "${USER_NAME}" --change-log "$@" >> "${SERVER_LOG}" 2>&1 &
  server_pid=$!
  local waited=0
  SERVER_PORT=""
  while [ "${waited}" -lt "${START_TIMEOUT_TENTHS}" ]; do
    SERVER_PORT="$(sed -n 's/.*listening on 127\.0\.0\.1:\([0-9][0-9]*\).*/\1/p' "${SERVER_LOG}" | head -1)"
    [ -n "${SERVER_PORT}" ] && return 0
    if ! kill -0 "${server_pid}" 2>/dev/null; then
      wait "${server_pid}" 2>/dev/null
      server_pid=""
      cat "${SERVER_LOG}" >&2
      fail "server exited before listening (db=${db})"
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  cat "${SERVER_LOG}" >&2
  fail "server did not start listening within $((START_TIMEOUT_TENTHS * 100))ms"
}

# 元 DB のサーバー（アーカイブ有効）を起動する。
start_src_server() {
  start_server "${SRC_DB}" --change-log-archive-dir "${ARCH}" \
    --change-log-archive-interval-secs "${ARCHIVE_INTERVAL_SECS}"
}

stop_server() {
  stop_pid "${server_pid}"
  server_pid=""
}

# 起動中のサーバーから `COUNT=<n> MAXID=<m>` を取り出し COUNT / MAXID を設定する。
COUNT=""
MAXID=""
read_counts() {
  local line
  line="$(python3 -I "${CLIENT}" count "${SERVER_PORT}" "${USER_NAME}" "${USER_PASS}")" \
    || fail "count query failed"
  COUNT="$(printf '%s' "${line}" | sed -n 's/^COUNT=\([0-9]*\) MAXID=.*/\1/p')"
  MAXID="$(printf '%s' "${line}" | sed -n 's/.* MAXID=\([0-9]*\)$/\1/p')"
  if ! [[ "${COUNT}" =~ ^[0-9]+$ ]] || ! [[ "${MAXID}" =~ ^[0-9]+$ ]]; then
    fail "could not parse counts: ${line}"
  fi
}

# DB を（停止状態で）サーバーに開かせ、件数を読んで停止する。引数は DB パス。
count_via_server() {
  start_server "$1"
  read_counts
  stop_server
}

# `verify --db` の終了コードと出力を VERIFY_RC / VERIFY_OUT へ、rows_scanned を VERIFY_ROWS へ設定する。
VERIFY_RC=0
VERIFY_OUT=""
VERIFY_ROWS=""
run_verify() {
  VERIFY_OUT="$("${BIN}" verify --db "$1" 2>&1)"
  VERIFY_RC=$?
  VERIFY_ROWS="$(printf '%s\n' "${VERIFY_OUT}" | sed -n 's/^rows_scanned=\([0-9]*\)$/\1/p')"
}

# 整合 DB（終了コード 0）であること。第 3 引数があればその rows_scanned とも一致すること。
expect_verify_ok() {
  local path="$1" what="$2" rows="${3:-}"
  run_verify "${path}"
  if [ "${VERIFY_RC}" -ne 0 ]; then
    echo "${VERIFY_OUT}" >&2
    fail "verify exit ${VERIFY_RC} (expected 0) for ${what}: ${path}"
  fi
  if [ -n "${rows}" ] && [ "${VERIFY_ROWS}" != "${rows}" ]; then
    echo "${VERIFY_OUT}" >&2
    fail "rows_scanned=${VERIFY_ROWS} differs from ${rows} for ${what}: ${path}"
  fi
}

# `change-log-verify --archive-dir <dir> [--db <path>]` を実行し、終了コードを CLV_RC、成功時の
# 先頭・末尾 LSN を CLV_FIRST / CLV_LAST（レコードが無ければ空）へ設定する。
CLV_RC=0
CLV_OUT=""
CLV_FIRST=""
CLV_LAST=""
run_clv() {
  CLV_OUT="$("${BIN}" change-log-verify --archive-dir "$@" 2>&1)"
  CLV_RC=$?
  CLV_FIRST="$(printf '%s\n' "${CLV_OUT}" | sed -n 's/.* lsn=\([0-9]*\)\.\.[0-9]*$/\1/p')"
  CLV_LAST="$(printf '%s\n' "${CLV_OUT}" | sed -n 's/.* lsn=[0-9]*\.\.\([0-9]*\)$/\1/p')"
}

# 出力ディレクトリの残骸検査。許可するのは、最終名（引数の接頭辞 glob）と一時名（`.fandhe-restore-*.tmp`）
# だけ。最終名の接頭辞は第 2 引数以降に複数渡せる。一時名は 0600 であること（全テナントのデータのコピーのため）も確かめる。
check_residue() {
  local dir="$1" entry name mode prefix
  shift
  for entry in "${dir}"/* "${dir}"/.[!.]*; do
    [ -e "${entry}" ] || continue
    name="$(basename "${entry}")"
    case "${name}" in
      .fandhe-restore-*.tmp)
        mode="$(stat -c '%a' "${entry}")"
        [ "${mode}" = "600" ] || fail "temp residue ${entry} has mode ${mode}, expected 600"
        ;;
      *)
        for prefix in "$@"; do
          case "${name}" in "${prefix}"*) continue 2 ;; esac
        done
        fail "unexpected entry in ${dir}: ${name}"
        ;;
    esac
  done
}

# ディレクトリ直下の一時名（第 2 引数の glob）の個数。
count_tmp() {
  find "$1" -maxdepth 1 -name "$2" | wc -l
}

# ディレクトリ直下の restore の一時名（厳密な形式。decoy は数えない）の個数。掃除が自分の uid を確かめるために
# 作る確認用ファイル（`.fandhe-sweep-probe-*`）は別の形式で、この正規表現に一致しない。掃除の最中の SIGKILL で
# 確認用ファイルが残っても「残骸の件数」に混ざらず、次の掃除が回収する（Issue #3094 のレビュー指摘への対応）。
count_restore_tmp() {
  find "$1" -maxdepth 1 -type f -regextype posix-extended \
    -regex '.*/\.fandhe-restore-[0-9]+(-[0-9a-f]{16})?-[0-9]+\.tmp' | wc -l
}

# ディレクトリ直下のファイルの「名前・サイズ・更新時刻」の一覧（入力が変わっていないことの比較用）。
dir_fingerprint() {
  find "$1" -maxdepth 1 -type f -printf '%f %s %T@\n' | sort
}

# 反復回数の方針。最低 ITERATIONS 回を実行し、さらに「公開前の一時名が残った状態で kill できた回（hit）」が
# 1 回以上になるまで続ける。打ち切りは ITERATIONS の 4 倍で、その時点で hit が 0 なら失敗にする。
should_continue() {
  local i="$1" hits="$2"
  [ "${i}" -lt $((ITERATIONS * 4)) ] || return 1
  [ "${i}" -lt "${ITERATIONS}" ] || [ "${hits}" -lt 1 ]
}

# ---------------------------------------------------------------------------
# 検査点: (LSN, 件数, 最大 id, 時刻)
# ---------------------------------------------------------------------------
PT_LSN=()
PT_COUNT=()
PT_MAXID=()
PT_TIME=()
# 時刻指定の検査が壁時計の飛び（NTP 補正・VM の時刻同期）に影響されないかを調べるための、検査点ごとの
# 壁時計（エポックからの ms）と単調時計（/proc/uptime。ms）。
PT_RT_MS=()
PT_MONO_MS=()

# サーバーを停止した直後に呼ぶ。時刻を採り、サーバーを再起動（修復・起動直後のアーカイブ）して件数を
# 読み、アーカイブの末尾 LSN が期待値に一致するまで待ってから停止し、検査点を追加する。
# 期待値は「直前の検査点の LSN + 件数の増分」（1 INSERT = 1 commit）。最初の検査点だけは期待値を
# 持たないため、末尾 LSN が 2 秒以上変わらないことで追いついたとみなす。
capture_point() {
  local idx="${#PT_LSN[@]}" t up _ rt_us
  # 単調時計と壁時計を fork なしで連続して採る（間に外部コマンドを挟まず、採取のゆらぎを避ける）。
  read -r up _ < /proc/uptime || fail "cannot read /proc/uptime"
  rt_us="${EPOCHREALTIME/./}"
  PT_RT_MS+=($((rt_us / 1000)))
  PT_MONO_MS+=($((${up/./} * 10)))
  # 時刻指定の点は採取時刻の 300ms 後にする。直前の commit（採取より前）との余裕を広げるため。
  # 次の周期の最初の commit がこの点より後になることは、関数の末尾で採取から 600ms 経つまで待って
  # 保証する（再起動・アーカイブ待ちが速い環境では、待たないと次周期の commit が点より前に入る）。
  t="$(date -u -d "@$(((rt_us + 300000) / 1000000)).$(printf '%06d' $(((rt_us + 300000) % 1000000)))" +%Y-%m-%dT%H:%M:%S.%6NZ)" \
    || fail "date could not format the checkpoint time"
  start_src_server
  read_counts
  local expect="" waited=0 stable=0 last=""
  if [ "${idx}" -gt 0 ]; then
    expect=$((PT_LSN[idx - 1] + COUNT - PT_COUNT[idx - 1]))
  fi
  while true; do
    run_clv "${ARCH}"
    if [ "${CLV_RC}" -eq 0 ] && [ -n "${CLV_LAST}" ]; then
      if [ -n "${expect}" ]; then
        [ "${CLV_LAST}" -le "${expect}" ] || fail "archive end ${CLV_LAST} is beyond the expected ${expect} (phantom commits)"
        [ "${CLV_LAST}" -eq "${expect}" ] && break
      else
        if [ "${CLV_LAST}" = "${last}" ]; then
          stable=$((stable + 1))
        else
          stable=0
          last="${CLV_LAST}"
        fi
        [ "${stable}" -ge 4 ] && break
      fi
    fi
    if [ "${waited}" -ge "${START_TIMEOUT_TENTHS}" ]; then
      echo "${CLV_OUT}" >&2
      fail "archive did not reach the expected end lsn (expect=${expect:-stable})"
    fi
    sleep 0.5
    waited=$((waited + 5))
  done
  stop_server
  # 検査点 1 の時点の元の系統の DB を複製しておく（phase 7 の増分の取得元。復元した DB は timeline が
  # 分岐する（Issue #3100）ため、元のベースへの増分は元の系統の DB からしか取れない）。
  if [ "${idx}" -eq 1 ]; then
    cp "${SRC_DB}" "${WORKDIR}/inc-src-original.redb" || fail "cannot copy the source DB at point 1"
  fi
  PT_LSN+=("${CLV_LAST}")
  PT_COUNT+=("${COUNT}")
  PT_MAXID+=("${MAXID}")
  PT_TIME+=("${t}")
  echo "point ${idx}: lsn=${CLV_LAST} count=${COUNT} max_id=${MAXID} time=${t}"
  # 次の周期の commit を時刻指定の点（採取の 300ms 後）より 300ms 以上後にする。
  local now_us
  now_us="${EPOCHREALTIME/./}"
  if [ "${now_us}" -lt $((rt_us + 600000)) ]; then
    sleep_ms $(((rt_us + 600000 - now_us) / 1000 + 1))
  fi
}

# ---------------------------------------------------------------------------
# 1. 種データ投入 → 検査点 0 → ベース
# ---------------------------------------------------------------------------
echo "=== phase 1: seed ${SEED_ROWS} rows, take the base backup ==="
start_src_server
python3 -I "${CLIENT}" seed "${SERVER_PORT}" "${USER_NAME}" "${USER_PASS}" "${SEED_ROWS}" \
  || fail "seed failed"
stop_server
capture_point
[ "${PT_COUNT[0]}" -eq "${SEED_ROWS}" ] || fail "seed count ${PT_COUNT[0]} differs from ${SEED_ROWS}"

BASE="${BAK_DIR}/base.redb"
"${BIN}" backup --db "${SRC_DB}" --out "${BASE}" >/dev/null || fail "base backup failed"
expect_verify_ok "${BASE}" "base backup"

# ---------------------------------------------------------------------------
# 2. クラッシュ周期: 追記の最中にサーバーを SIGKILL し、検査点を採る
# ---------------------------------------------------------------------------
# 1 周期: サーバーを起動して追記し、追記の最中に SIGKILL して検査点を採る。引数は周期の通し番号。
cycle_kills=0
crash_cycle() {
  local cycle="$1" prev start_id waited last_ack acked grew
  prev=$((${#PT_LSN[@]} - 1))
  start_id=$((PT_MAXID[prev] + 1))
  start_src_server
  : > "${CLIENT_LOG}"
  python3 -I "${CLIENT}" append "${SERVER_PORT}" "${USER_NAME}" "${USER_PASS}" "${start_id}" \
    >> "${CLIENT_LOG}" 2>&1 &
  client_pid=$!
  waited=0
  while ! grep -q '^ACK ' "${CLIENT_LOG}" 2>/dev/null; do
    if [ "${waited}" -ge "${START_TIMEOUT_TENTHS}" ] || ! kill -0 "${client_pid}" 2>/dev/null; then
      cat "${CLIENT_LOG}" >&2
      fail "append client did not report an acknowledged insert (cycle ${cycle})"
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  # 追記を少し進めてから kill する（コミット中の書き込みがある確率を上げる）。
  sleep_ms $((300 + RANDOM % 500))
  # 追記クライアントが生きている間にサーバーを kill できた周期だけを「書き込み中の kill」と数える。
  if kill -0 "${client_pid}" 2>/dev/null; then
    cycle_kills=$((cycle_kills + 1))
  fi
  stop_server
  stop_pid "${client_pid}"
  client_pid=""
  last_ack="$(sed -n 's/^ACK \([0-9]*\)$/\1/p' "${CLIENT_LOG}" | tail -1)"
  [[ "${last_ack}" =~ ^[0-9]+$ ]] || fail "no acknowledged append in cycle ${cycle}"
  acked=$((last_ack - start_id + 1))

  capture_point
  grew=$((PT_COUNT[prev + 1] - PT_COUNT[prev]))
  # 確認済みの追記はすべて残り、commit 済みで未確認の行は高々 1 行。
  if [ "${grew}" -lt "${acked}" ] || [ "${grew}" -gt $((acked + 1)) ]; then
    fail "cycle ${cycle}: rows grew by ${grew}, outside [${acked}, $((acked + 1))] (data loss or phantom rows)"
  fi
  [ "${PT_MAXID[prev + 1]}" -ge "${last_ack}" ] || fail "cycle ${cycle}: max id ${PT_MAXID[prev + 1]} is below the last acknowledged id ${last_ack}"
  # 1 INSERT = 1 commit、id は連番。
  [ $((PT_LSN[prev + 1] - PT_LSN[prev])) -eq "${grew}" ] || fail "cycle ${cycle}: lsn advanced by $((PT_LSN[prev + 1] - PT_LSN[prev])) for ${grew} rows"
  [ $((PT_MAXID[prev + 1] - PT_MAXID[prev])) -eq "${grew}" ] || fail "cycle ${cycle}: ids are not contiguous"
  echo "cycle ${cycle}: acked=${acked} committed=${grew}"
}

# 時刻指定の検査に使える検査点 k（1 以上、最後の点ではない）を TIME_K に設定する。前後の検査点との間で
# 壁時計が単調時計と 30ms を超えてずれていない（壁時計の飛びが無い）ことを条件にする。無ければ空。
TIME_K=""
find_time_point() {
  local k j drift ok last=$((${#PT_LSN[@]} - 1))
  TIME_K=""
  for k in $(seq 1 $((last - 1))); do
    ok=1
    for j in $((k - 1)) "${k}"; do
      drift=$(((PT_RT_MS[j + 1] - PT_RT_MS[j]) - (PT_MONO_MS[j + 1] - PT_MONO_MS[j])))
      [ "${drift#-}" -le 30 ] || ok=0
    done
    if [ "${ok}" -eq 1 ]; then
      TIME_K="${k}"
      return 0
    fi
  done
  return 1
}

echo "=== phase 2: ${CYCLES} crash cycles (SIGKILL the server during appends) ==="
for cycle in $(seq 1 "${CYCLES}"); do
  crash_cycle "${cycle}"
done
# 壁時計が飛んだ環境では時刻指定の検査が成り立たないため、飛びの無い検査点が採れるまで周期を足す
# （上限 5 回。通常の環境では足さない）。
extra=0
while ! find_time_point && [ "${extra}" -lt 5 ]; do
  extra=$((extra + 1))
  echo "wall clock stepped between points; running an extra crash cycle (${extra})"
  crash_cycle $((CYCLES + extra))
done
[ -n "${TIME_K}" ] || fail "the wall clock kept stepping; no usable point for the --until-time check"
LAST=$((${#PT_LSN[@]} - 1))
TOTAL_CYCLES=$((CYCLES + extra))
if [ "${cycle_kills}" -lt 1 ]; then
  fail "the server was never killed while the appender was running (test is vacuous)"
fi
echo "server kills during appends: ${cycle_kills}/${TOTAL_CYCLES}"

# 公開済みのアーカイブの残骸検査（セグメントの最終名か、書き出し途中の一時名だけ）。
for entry in "${ARCH}"/* "${ARCH}"/.[!.]*; do
  [ -e "${entry}" ] || continue
  case "$(basename "${entry}")" in
    fandhe-cl-*.seg | .fandhe-seg-*.tmp) ;;
    *) fail "unexpected entry in the archive directory: $(basename "${entry}")" ;;
  esac
done

# ---------------------------------------------------------------------------
# 3. 停止後の change-log-verify
# ---------------------------------------------------------------------------
echo "=== phase 3: change-log-verify after the crashes ==="
run_clv "${ARCH}"
[ "${CLV_RC}" -eq 0 ] || { echo "${CLV_OUT}" >&2; fail "change-log-verify exit ${CLV_RC} (expected 0)"; }
[ "${CLV_FIRST}" = "1" ] || fail "archive starts at lsn ${CLV_FIRST}, expected 1"
[ "${CLV_LAST}" = "${PT_LSN[LAST]}" ] || fail "archive ends at lsn ${CLV_LAST}, expected ${PT_LSN[LAST]}"
SEG_COUNT="$(find "${ARCH}" -maxdepth 1 -name 'fandhe-cl-*.seg' | wc -l)"
[ "${SEG_COUNT}" -ge 4 ] || fail "only ${SEG_COUNT} archive segments (tamper cases need 4+)"
# 異常終了した DB のコピーとの突き合わせ（DB を直接読み取り専用で開くと修復待ちで拒否されるため複製を使う）。
SRC_COPY="${BAK_DIR}/source-copy.redb"
"${BIN}" backup --db "${SRC_DB}" --out "${SRC_COPY}" >/dev/null || fail "source copy backup failed"
run_clv "${ARCH}" --db "${SRC_COPY}"
[ "${CLV_RC}" -eq 0 ] || { echo "${CLV_OUT}" >&2; fail "change-log-verify --db exit ${CLV_RC} (expected 0)"; }
echo "archive ok: segments=${SEG_COUNT} lsn=${CLV_FIRST}..${CLV_LAST}"

# ---------------------------------------------------------------------------
# 4. PITR の復元と一致検査
# ---------------------------------------------------------------------------
# 引数: <復元先> <restore の追加引数...>。終了コードを RESTORE_RC に設定する。
RESTORE_RC=0
RESTORE_ERR=""
restore_to() {
  local dest="$1"
  shift
  RESTORE_ERR="$("${BIN}" restore --from "${BASE}" --db "${dest}" "$@" 2>&1 >/dev/null)"
  RESTORE_RC=$?
}

# 想定外の終了コードのとき、診断のため restore の標準エラーを出して失敗にする。
restore_fail() {
  echo "${RESTORE_ERR}" >&2
  fail "$*"
}

# 復元先が整合し（verify 0）、サーバーで読んだ件数・最大 id が期待値と一致すること。
expect_state() {
  local dest="$1" what="$2" want_count="$3" want_maxid="$4"
  expect_verify_ok "${dest}" "${what}"
  count_via_server "${dest}"
  if [ "${COUNT}" != "${want_count}" ] || [ "${MAXID}" != "${want_maxid}" ]; then
    fail "${what}: count=${COUNT} max_id=${MAXID}, expected ${want_count}/${want_maxid}"
  fi
  # 共有の一時領域（tmpfs）を圧迫しないよう、検査済みの復元先はすぐ消す（rows_scanned は呼び出し側が
  # VERIFY_ROWS から読む）。
  rm -f "${dest}"
}

echo "=== phase 4: restore --until-lsn / --until-time and compare with the recorded states ==="
t0="$(now_ms)"
for i in $(seq 1 "${LAST}"); do
  dest="${RST_DIR}/lsn-${i}.redb"
  restore_to "${dest}" --change-log-archive-dir "${ARCH}" --until-lsn "${PT_LSN[i]}"
  [ "${RESTORE_RC}" -eq 0 ] || restore_fail "restore --until-lsn ${PT_LSN[i]} exit ${RESTORE_RC} (expected 0)"
  expect_state "${dest}" "restore to point ${i}" "${PT_COUNT[i]}" "${PT_MAXID[i]}"
  if [ "${i}" -eq "${LAST}" ]; then
    LAST_RESTORED_ROWS="${VERIFY_ROWS}"
  fi
  # 周期の中間（commit 境界のちょうど途中）: ベースの次の点から 1 commit ずつ進んだ状態になる。
  prev=$((i - 1))
  mid=$(((PT_LSN[prev] + PT_LSN[i]) / 2))
  if [ "${mid}" -gt "${PT_LSN[prev]}" ]; then
    dest="${RST_DIR}/mid-${i}.redb"
    restore_to "${dest}" --change-log-archive-dir "${ARCH}" --until-lsn "${mid}"
    [ "${RESTORE_RC}" -eq 0 ] || restore_fail "restore --until-lsn ${mid} exit ${RESTORE_RC} (expected 0)"
    delta=$((mid - PT_LSN[prev]))
    expect_state "${dest}" "restore to the middle of cycle ${i}" \
      $((PT_COUNT[prev] + delta)) $((PT_MAXID[prev] + delta))
  fi
done
RESTORE_MS=$(($(now_ms) - t0))
echo "restored ${LAST} points and ${LAST} mid-cycle points in ${RESTORE_MS}ms"

# 時刻指定: 検査点 TIME_K の時刻（その周期の停止後・次の周期の開始前）まで。
dest="${RST_DIR}/time-${TIME_K}.redb"
restore_to "${dest}" --change-log-archive-dir "${ARCH}" --until-time "${PT_TIME[TIME_K]}"
[ "${RESTORE_RC}" -eq 0 ] || restore_fail "restore --until-time ${PT_TIME[TIME_K]} exit ${RESTORE_RC} (expected 0)"
expect_state "${dest}" "restore until time of point ${TIME_K}" "${PT_COUNT[TIME_K]}" "${PT_MAXID[TIME_K]}"
# 最後のコミットより後の時刻（遠い未来）は、範囲外として拒否される（末尾まで黙って適用しない）。
dest="${RST_DIR}/time-late.redb"
restore_to "${dest}" --change-log-archive-dir "${ARCH}" --until-time "2999-01-01T00:00:00Z"
[ "${RESTORE_RC}" -eq 1 ] || restore_fail "restore --until-time past the last commit exit ${RESTORE_RC} (expected 1)"
[ ! -e "${dest}" ] || fail "rejected time restore published ${dest}"

# 範囲外の指定は拒否され、何も公開されない（ベースの終点以前、ログの最大 LSN より後）。
for bad in "${PT_LSN[0]}" $((PT_LSN[LAST] + 1)); do
  dest="${RST_DIR}/bad-${bad}.redb"
  restore_to "${dest}" --change-log-archive-dir "${ARCH}" --until-lsn "${bad}"
  [ "${RESTORE_RC}" -eq 1 ] || restore_fail "restore --until-lsn ${bad} (out of range) exit ${RESTORE_RC}, expected 1"
  [ ! -e "${dest}" ] || fail "out-of-range restore published ${dest}"
done
check_residue "${RST_DIR}" "lsn-" "mid-" "time-" "bad-"
echo "restored states match the recorded states; out-of-range requests are rejected"

# ---------------------------------------------------------------------------
# 5. PITR の restore 中の SIGKILL
# ---------------------------------------------------------------------------
# kill の遅延幅は restore 単体の所要時間で決める（サーバー起動・件数読み出しを含む phase 4 の平均では広すぎる）。
t0="$(now_ms)"
restore_to "${RST_DIR}/timing.redb" --change-log-archive-dir "${ARCH}" --until-lsn "${PT_LSN[LAST]}"
PER_RESTORE_MS=$(($(now_ms) - t0))
[ "${RESTORE_RC}" -eq 0 ] || restore_fail "timing restore exit ${RESTORE_RC} (expected 0)"
rm -f "${RST_DIR}/timing.redb"
restore_limit_ms=$((PER_RESTORE_MS * 3 / 2))
[ "${restore_limit_ms}" -ge 20 ] || restore_limit_ms=20
echo "=== phase 5: SIGKILL during PITR restore (${ITERATIONS}+ iterations, delay 0-${restore_limit_ms}ms) ==="
ARCH_FP_BEFORE="$(dir_fingerprint "${ARCH}")"
BASE_FP_BEFORE="$(dir_fingerprint "${BAK_DIR}")"
restore_killed=0
restore_hits=0
restore_completed=0
restore_residue=0
RST_INTERRUPTED=""
# 形式に一致しない名前は掃除されない（Issue #3094 の受入基準）。
# 掃除は他者が書けるディレクトリでは見送るため、umask に依らず 0755 にする。
chmod 755 "${RSTK_DIR}"
printf 'decoy' >"${RSTK_DIR}/.fandhe-restore-decoy.tmp"
chmod 600 "${RSTK_DIR}/.fandhe-restore-decoy.tmp"
i=0
while should_continue "${i}" "${restore_hits}"; do
  i=$((i + 1))
  out="${RSTK_DIR}/rst-${i}.redb"
  "${BIN}" restore --from "${BASE}" --db "${out}" --change-log-archive-dir "${ARCH}" \
    --until-lsn "${PT_LSN[LAST]}" >/dev/null 2>&1 &
  job_pid=$!
  sleep_ms $((RANDOM % (restore_limit_ms + 1)))
  kill -9 "${job_pid}" 2>/dev/null
  wait "${job_pid}" 2>/dev/null
  status=$?
  job_pid=""
  if [ "${status}" -eq 137 ]; then
    restore_killed=$((restore_killed + 1))
    if [ ! -e "${out}" ] && [ "$(count_restore_tmp "${RSTK_DIR}")" -ge 1 ]; then
      restore_hits=$((restore_hits + 1))
      [ -n "${RST_INTERRUPTED}" ] || RST_INTERRUPTED="${out}"
    fi
  else
    case "${status}" in
      0 | 3) restore_completed=$((restore_completed + 1)) ;;
      *) fail "restore iteration ${i} exited with ${status} (expected 0, 3 or 137)" ;;
    esac
  fi
  if [ -e "${out}" ]; then
    expect_verify_ok "${out}" "PITR restore output of iteration ${i}" "${LAST_RESTORED_ROWS}"
  fi
  check_residue "${RSTK_DIR}" "rst-"
  # 一時名の残骸は次の restore が自動で掃除する（Issue #3094）。反復をまたいで残るのは高々 1 件で、decoy は触られない。
  restore_residue=$((restore_residue + $(count_restore_tmp "${RSTK_DIR}")))
  [ "$(count_restore_tmp "${RSTK_DIR}")" -le 1 ] || fail "more than one stale restore temp name remains after iteration ${i}"
  [ -f "${RSTK_DIR}/.fandhe-restore-decoy.tmp" ] || fail "decoy temp name was removed by restore iteration ${i}"
  # 入力（ベース・アーカイブ）は読み取りだけで変わらない。
  [ "$(dir_fingerprint "${ARCH}")" = "${ARCH_FP_BEFORE}" ] || fail "the archive changed during restore iteration ${i}"
  [ "$(dir_fingerprint "${BAK_DIR}")" = "${BASE_FP_BEFORE}" ] || fail "the base directory changed during restore iteration ${i}"
done
echo "restore kills: iterations=${i} killed=${restore_killed} hit_mid_run=${restore_hits} completed_before_kill=${restore_completed}"
if [ $((restore_hits * 4)) -lt "${ITERATIONS}" ]; then
  echo "WARN: only ${restore_hits} of ${i} restores were killed mid-run (delay range may not fit this machine)" >&2
fi
if [ "${restore_hits}" -lt 1 ] || [ -z "${RST_INTERRUPTED}" ]; then
  fail "no PITR restore was killed after starting and before publishing (test is vacuous)"
fi
echo "restore temp residue files seen after kills (swept by the next restore): ${restore_residue}"

# 中断されて最終名が無かった復元先と同じパスへ取り直せ、検査点と一致する。
[ ! -e "${RST_INTERRUPTED}" ] || fail "interrupted restore target unexpectedly exists: ${RST_INTERRUPTED}"
restore_to "${RST_INTERRUPTED}" --change-log-archive-dir "${ARCH}" --until-lsn "${PT_LSN[LAST]}"
[ "${RESTORE_RC}" -eq 0 ] || restore_fail "restore retry to the interrupted target exit ${RESTORE_RC}"
expect_state "${RST_INTERRUPTED}" "restore retry to the interrupted target" \
  "${PT_COUNT[LAST]}" "${PT_MAXID[LAST]}"
# 完走した restore のあとは、SIGKILL の残骸が自動で掃除されて 0 件になり、decoy だけが残る。
[ "$(count_restore_tmp "${RSTK_DIR}")" -eq 0 ] || fail "stale restore temp names were not swept by the next restore"
[ -f "${RSTK_DIR}/.fandhe-restore-decoy.tmp" ] || fail "decoy temp name was removed by restore"

# ---------------------------------------------------------------------------
# 6. ログの欠落・改ざんの拒否
# ---------------------------------------------------------------------------
echo "=== phase 6: tampered archives are rejected by change-log-verify and restore ==="
mapfile -t SEGS < <(find "${ARCH}" -maxdepth 1 -name 'fandhe-cl-*.seg' -printf '%f\n' | sort)
[ "${#SEGS[@]}" -ge 4 ] || fail "need 4+ segments, found ${#SEGS[@]}"
LAST_SEG="${SEGS[$((${#SEGS[@]} - 1))]}"
# 停止点より後ろのセグメントの改ざん（flip-after-stop・truncate-after-stop）が、停止点（検査点 1）の
# 復元に必要なセグメントを壊すだけの検査にならないよう、末尾セグメントの先頭 LSN が停止点より後ろで
# あることを名前（`...-<先頭 LSN>-<末尾 LSN>.seg`）から確かめる。
LAST_SEG_FIRST="$(printf '%s\n' "${LAST_SEG}" | sed -n 's/.*-\([0-9][0-9]*\)-[0-9][0-9]*\.seg$/\1/p')"
[[ "${LAST_SEG_FIRST}" =~ ^[0-9]+$ ]] || fail "could not parse the first lsn of ${LAST_SEG}"
[ $((10#${LAST_SEG_FIRST})) -gt "${PT_LSN[1]}" ] || fail "the last segment starts at lsn ${LAST_SEG_FIRST}, not after the stop point ${PT_LSN[1]}"

# ファイルの指定オフセットの 1 バイトを反転する。
flip_byte() {
  local file="$1" off="$2" orig new
  orig="$(od -An -tu1 -j "${off}" -N1 "${file}" | tr -d ' ')"
  [[ "${orig}" =~ ^[0-9]+$ ]] || fail "could not read a byte of ${file} at ${off}"
  new=$((orig ^ 255))
  printf '%b' "\\$(printf '%03o' "${new}")" | dd of="${file}" bs=1 seek="${off}" conv=notrunc 2>/dev/null \
    || fail "could not rewrite a byte of ${file}"
}

# 改ざんの 1 ケース。引数: <名前> <until_lsn> <change-log-verify の期待: reject〔単体で拒否〕|tail〔--db との突き合わせで拒否〕> <改ざん関数名>。
# アーカイブの複製を作って改ざんし、verify（reject のとき）と restore の拒否・復元先の空を確かめる。
tamper_case() {
  local name="$1" until_lsn="$2" verify_expect="$3" mutate="$4" want_msg="$5"
  local copy="${TAMP_DIR}/${name}" dest_dir="${TAMP_DIR}/${name}-out"
  cp -a "${ARCH}" "${copy}" || fail "copy of the archive failed (${name})"
  mkdir "${dest_dir}"
  chmod 700 "${dest_dir}"
  "${mutate}" "${copy}"
  if [ "${verify_expect}" = "reject" ]; then
    run_clv "${copy}"
    [ "${CLV_RC}" -eq 1 ] || { echo "${CLV_OUT}" >&2; fail "tamper ${name}: change-log-verify exit ${CLV_RC}, expected 1"; }
  elif [ "${verify_expect}" = "tail" ]; then
    # 末尾の欠落はアーカイブ単体では検出できない。DB のコピーとの突き合わせ（--db）で検出される。
    run_clv "${copy}" --db "${SRC_COPY}"
    [ "${CLV_RC}" -eq 1 ] || { echo "${CLV_OUT}" >&2; fail "tamper ${name}: change-log-verify --db exit ${CLV_RC}, expected 1"; }
  fi
  restore_to "${dest_dir}/rst.redb" --change-log-archive-dir "${copy}" --until-lsn "${until_lsn}"
  [ "${RESTORE_RC}" -eq 1 ] || restore_fail "tamper ${name}: restore exit ${RESTORE_RC}, expected 1"
  # 拒否の理由がアーカイブ起因であること（固定の英語文の識別語。引数の誤りなど別の理由での終了コード 1 と区別する）。
  case "${RESTORE_ERR}" in
    *"${want_msg}"*) ;;
    *) restore_fail "tamper ${name}: restore error does not mention '${want_msg}'" ;;
  esac
  if [ -n "$(find "${dest_dir}" -mindepth 1 -print -quit)" ]; then
    fail "tamper ${name}: a rejected restore left entries in the destination directory"
  fi
  rm -rf "${copy}"
  echo "tamper ${name}: rejected (verify=${verify_expect}, restore exit 1, nothing published)"
}

mutate_none() { :; }
mutate_delete_mid() { rm -f "$1/${SEGS[1]}"; }
mutate_flip_mid() {
  local size
  size="$(stat -c '%s' "$1/${SEGS[1]}")"
  flip_byte "$1/${SEGS[1]}" $((size / 2))
}
mutate_swap() {
  mv "$1/${SEGS[1]}" "$1/swap.tmp" && mv "$1/${SEGS[2]}" "$1/${SEGS[1]}" && mv "$1/swap.tmp" "$1/${SEGS[2]}"
}
mutate_flip_last() {
  local size
  size="$(stat -c '%s' "$1/${LAST_SEG}")"
  flip_byte "$1/${LAST_SEG}" $((size / 2))
}
mutate_truncate_last() {
  local size
  size="$(stat -c '%s' "$1/${LAST_SEG}")"
  truncate -s $((size - 1)) "$1/${LAST_SEG}"
}
mutate_delete_last() { rm -f "$1/${LAST_SEG}"; }

# 対照: 無改ざんの複製は同じ条件で復元できる（拒否が引数の誤りでないことの確認）。
copy="${TAMP_DIR}/control"
cp -a "${ARCH}" "${copy}" || fail "copy of the archive failed (control)"
run_clv "${copy}"
[ "${CLV_RC}" -eq 0 ] || fail "control archive copy does not verify"
# 拒否ケースの停止点（検査点 1）と最新（改ざん全般の既定）の両方で成功する。
restore_to "${TAMP_DIR}/control-rst.redb" --change-log-archive-dir "${copy}" --until-lsn "${PT_LSN[1]}"
[ "${RESTORE_RC}" -eq 0 ] || restore_fail "control restore exit ${RESTORE_RC} (expected 0)"
rm -f "${TAMP_DIR}/control-rst.redb"
restore_to "${TAMP_DIR}/control-rst.redb" --change-log-archive-dir "${copy}" --until-lsn "${PT_LSN[LAST]}"
[ "${RESTORE_RC}" -eq 0 ] || restore_fail "control restore to the last lsn exit ${RESTORE_RC} (expected 0)"
rm -rf "${copy}" "${TAMP_DIR}/control-rst.redb"

tamper_case delete-mid "${PT_LSN[LAST]}" reject mutate_delete_mid "gap or overlap"
tamper_case flip-mid "${PT_LSN[LAST]}" reject mutate_flip_mid "checksum mismatch"
tamper_case swap "${PT_LSN[LAST]}" reject mutate_swap "malformed"
# 停止点（検査点 1）より後ろのセグメントの破損・切り詰めも拒否する。
tamper_case flip-after-stop "${PT_LSN[1]}" reject mutate_flip_last "checksum mismatch"
tamper_case truncate-after-stop "${PT_LSN[1]}" reject mutate_truncate_last "truncated"
# 末尾の削除はアーカイブ単体では検出できないため、verify は --db との突き合わせで確かめる。最新までの
# 復元は、ログの最大 LSN が足りず範囲外として拒否される。
tamper_case delete-last "${PT_LSN[LAST]}" tail mutate_delete_last "outside the range"
# 元のアーカイブは改ざん試験の影響を受けていない。
[ "$(dir_fingerprint "${ARCH}")" = "${ARCH_FP_BEFORE}" ] || fail "the original archive changed during tamper cases"

# ---------------------------------------------------------------------------
# 7. 増分バックアップの併用
# ---------------------------------------------------------------------------
echo "=== phase 7: incremental backup combined with the archive ==="
# 検査点 1 の時点の元の系統の DB（phase 2 で複製済み）から増分を取る。復元した DB は timeline が分岐し
# ベースと別の系統になるため、増分の取得元には使えない。
MIDPT=1
INC="${BAK_DIR}/inc.seg"
INC_SRC="${WORKDIR}/inc-src-original.redb"
[ -f "${INC_SRC}" ] || fail "the original-line source copy at point 1 is missing"
# 複製元はサーバーを SIGKILL した後の DB で、読み取り専用では開けない。フルバックアップ（元の系統のまま
# 整合した複製を作る）を通して停止中の DB にする。
INC_SRC_CLEAN="${WORKDIR}/inc-src-clean.redb"
"${BIN}" backup --db "${INC_SRC}" --out "${INC_SRC_CLEAN}" >/dev/null \
  || fail "cannot take a clean copy of the point ${MIDPT} source"
INC_SRC="${INC_SRC_CLEAN}"
"${BIN}" backup-incremental --db "${INC_SRC}" --base "${BASE}" --out "${INC}" >/dev/null \
  || fail "backup-incremental from point ${MIDPT} failed"
# 増分の末尾（検査点 MIDPT）より後ろをアーカイブから補って最新まで。
dest="${RST_DIR}/inc-archive.redb"
restore_to "${dest}" --incremental "${INC}" --change-log-archive-dir "${ARCH}" --until-lsn "${PT_LSN[LAST]}"
[ "${RESTORE_RC}" -eq 0 ] || restore_fail "restore --incremental + archive exit ${RESTORE_RC} (expected 0)"
expect_state "${dest}" "restore incremental + archive to the last point" "${PT_COUNT[LAST]}" "${PT_MAXID[LAST]}"
# 増分だけ（アーカイブなし）で増分の範囲内の LSN まで。
inc_mid=$(((PT_LSN[0] + PT_LSN[MIDPT]) / 2))
dest="${RST_DIR}/inc-only.redb"
restore_to "${dest}" --incremental "${INC}" --until-lsn "${inc_mid}"
[ "${RESTORE_RC}" -eq 0 ] || restore_fail "restore --incremental only exit ${RESTORE_RC} (expected 0)"
delta=$((inc_mid - PT_LSN[0]))
expect_state "${dest}" "restore incremental only to an lsn inside it" \
  $((PT_COUNT[0] + delta)) $((PT_MAXID[0] + delta))
# 改ざんした増分は拒否され、何も公開されない。
BAD_INC="${TAMP_DIR}/inc-bad.seg"
cp -a "${INC}" "${BAD_INC}"
flip_byte "${BAD_INC}" $(($(stat -c '%s' "${BAD_INC}") / 2))
mkdir "${TAMP_DIR}/inc-out"
chmod 700 "${TAMP_DIR}/inc-out"
restore_to "${TAMP_DIR}/inc-out/rst.redb" --incremental "${BAD_INC}" --change-log-archive-dir "${ARCH}" \
  --until-lsn "${PT_LSN[LAST]}"
[ "${RESTORE_RC}" -ne 0 ] || fail "restore with a tampered incremental unexpectedly succeeded"
if [ -n "$(find "${TAMP_DIR}/inc-out" -mindepth 1 -print -quit)" ]; then
  fail "a rejected incremental restore left entries in the destination directory"
fi
echo "incremental: archive-supplemented and incremental-only restores match; a tampered incremental is rejected"

echo "workdir size at the end: $(du -sm "${WORKDIR}" | cut -f1)MB"
echo "crash_test_pitr.sh: OK (points=$((LAST + 1)), server kills=${cycle_kills}/${TOTAL_CYCLES}, restore hits=${restore_hits}/${restore_killed}, segments=${SEG_COUNT}, per-restore~${PER_RESTORE_MS}ms)"
