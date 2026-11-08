  # ACK を受けた操作列（docs の INSERT と kv の INSERT・UPDATE・DELETE）の結果が、
  # 両テナントとも再起動後のプライマリに残っている。
  if [ "${last_a}" -ge 0 ]; then
    on_primary alice ids-present docs $((WA_BASE + last_a)) $((WA_BASE + last_a)) > /dev/null \
      || fail "an acknowledged commit was lost after primary restart (cycle ${i})"
    on_primary alice churn-verify "${WA_BASE}" "${last_a}" > /dev/null \
      || fail "an acknowledged alice operation was lost after primary restart (cycle ${i})"
  fi
  if [ "${last_b}" -ge 0 ]; then
    on_primary bob ids-present docs $((WB_BASE + last_b)) $((WB_BASE + last_b)) > /dev/null \
      || fail "an acknowledged bob commit was lost after primary restart (cycle ${i})"
    on_primary bob churn-verify "${WB_BASE}" "${last_b}" > /dev/null \
      || fail "an acknowledged bob operation was lost after primary restart (cycle ${i})"
  fi
#!/usr/bin/env bash
# Issue #2178（RECOVER-15 ポインタ。親 #2171）のレプリケーションの障害注入・追従検証の回帰テスト。
#
# リリースバイナリの実プロセス（プライマリ `--change-log --replication-bind` と、読み取りレプリカ
# `--replica-primary`）を、書き込み（alice・bob の 2 テナント）の最中に障害へさらし、レプリカが自動で追従を
# 再開して「全表の内容がプライマリと一致する」ことを確かめる。レプリカとプライマリの間にはループバックの
# TCP 中継（scripts/crash_test_replication_proxy.py）を置き、プライマリの再起動でポートが変わっても
# レプリカの接続先を固定にし、両プロセスを生かしたままの接続断・通信の停滞も注入する。
#
# 段:
#   0. 準備: 3 表を作り、フルバックアップをレプリカのベースにして起動し、追従を確かめる。
#   1. レプリカの SIGKILL（書き込み中）を繰り返す。停止中の DB が verify を通り、再接続の適用位置が
#      単調非減少（先頭からやり直さない）こと。
#   2. 中継の接続断（両プロセスは生存）を繰り返す。レプリカの再起動なしで再接続すること。
#   3. 遅延: 中継の停滞・レプリカの SIGSTOP でプライマリ側の ack 待ちの上限を発火させ、再接続すること。
#   4. DDL（表の追加・列の追加）が書き込み中でも複製され、列名・内容が一致すること。
#   5. プライマリの SIGKILL と再起動を繰り返す。確認済み（ACK）の commit が失われず、レプリカが
#      プライマリより先に進んで停止しないこと。
#   6. 同期レプリケーション（#2245）: ack を受けた commit が、プライマリを SIGKILL した後もレプリカにあること。
#   7. 終了時: 両 DB が verify を通ること。
# 各段の後は、書き込みを止め、目印の行がレプリカから見えるまで待ち（適用は LSN 順なので、見えれば
# それ以前の commit は適用済み）、テナントごとに表の列名・全行の sha256 を比べる。比較は表の内容を
# ログへ出さずハッシュだけ出す。
# 空虚な成功を避けるため、障害が「書き込みの最中」に当たった回数と、各種の切断が実際に観測された回数を
# 数え、0 回なら失敗とする。
# Makefile の crash-test-replication ターゲット（crash-test の前提）から呼ばれ、CI では既存の crash-test
# ジョブで実行される。
#
# 使い方: scripts/crash_test_replication.sh [レプリカ kill の反復（既定 3）] [接続断の反復（既定 3）] [プライマリ kill の反復（既定 3）]

set -u

REPLICA_KILLS="${1:-3}"
CUTS="${2:-3}"
PRIMARY_KILLS="${3:-3}"
for v in "${REPLICA_KILLS}" "${CUTS}" "${PRIMARY_KILLS}"; do
  if ! [[ "${v}" =~ ^[0-9]+$ ]] || [ "${v}" -lt 1 ] || [ "${v}" -gt 50 ]; then
    echo "ERROR: arguments must be integers in 1..50, got: ${v}" >&2
    exit 1
  fi
done

[ -r /proc/uptime ] || { echo "ERROR: Linux is required" >&2; exit 1; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${REPO_ROOT}/target/release/wire-server"
CLIENT="${REPO_ROOT}/scripts/crash_test_backup_restore_client.py"
PROXY="${REPO_ROOT}/scripts/crash_test_replication_proxy.py"

if ! command -v python3 >/dev/null 2>&1; then
  echo "ERROR: python3 is required (pg wire client and proxy helpers)" >&2
  exit 1
fi

echo "building wire-server (release)"
if ! (cd "${REPO_ROOT}" && cargo build --release -p fandhe-db-wire-server --bin wire-server); then
  echo "ERROR: failed to build wire-server" >&2
  exit 1
fi
[ -x "${BIN}" ] || { echo "ERROR: wire-server binary not found at ${BIN}" >&2; exit 1; }

WORKDIR="$(mktemp -d)"
[ -d "${WORKDIR}" ] || { echo "ERROR: mktemp -d failed" >&2; exit 1; }

# バックグラウンドの子の PID（trap 設置前に初期化する）。
primary_pid=""
replica_pid=""
proxy_pid=""
wa_pid=""
wb_pid=""
sw_pid=""

stop_pid() {
  local pid="$1"
  if [ -n "${pid}" ]; then
    kill -9 "${pid}" 2>/dev/null
    wait "${pid}" 2>/dev/null
  fi
}

cleanup() {
  stop_pid "${wa_pid}"
  stop_pid "${wb_pid}"
  stop_pid "${sw_pid}"
  stop_pid "${replica_pid}"
  stop_pid "${primary_pid}"
  stop_pid "${proxy_pid}"
  wa_pid=""; wb_pid=""; sw_pid=""; replica_pid=""; primary_pid=""; proxy_pid=""
  rm -rf "${WORKDIR}"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

echo "workdir: ${WORKDIR}"

# 失敗時は各ログの末尾を出す（ログは件数・LSN・固定の文言だけで、行の中身は含まない）。
fail() {
  echo "ERROR: $*" >&2
  local f
  for f in "${WORKDIR}"/primary_*.log "${WORKDIR}"/replica_*.log "${WORKDIR}"/proxy.log; do
    [ -f "${f}" ] || continue
    echo "--- tail of $(basename "${f}")" >&2
    tail -n 8 "${f}" >&2
  done
  exit 1
}

# 資格情報（ダミーのテスト用の値）。
PW_ALICE="pw-alice-test"
PW_BOB="pw-bob-test"
PW_ROLE="pw-replica-test"
USERS="${WORKDIR}/users.txt"
ROLES="${WORKDIR}/replication-roles.txt"
CRED="${WORKDIR}/replica-credentials"
TARGET_FILE="${WORKDIR}/proxy-target"
PRIMARY_DB="${WORKDIR}/primary.redb"
REPLICA_DB="${WORKDIR}/replica.redb"
SOCK_DIR="${WORKDIR}/s"
SOCK="${SOCK_DIR}/b.sock"
mkdir "${SOCK_DIR}"
chmod 700 "${SOCK_DIR}"

phc_of() { printf '%s\n' "$1" | "${BIN}" hash-password; }
{
  printf 'alice:tenant-a:%s\n' "$(phc_of "${PW_ALICE}")"
  printf 'bob:tenant-b:%s\n' "$(phc_of "${PW_BOB}")"
} > "${USERS}" || fail "users file"
printf 'replica-1:%s\n' "$(phc_of "${PW_ROLE}")" > "${ROLES}" || fail "roles file"
printf 'replica-1:%s\n' "${PW_ROLE}" > "${CRED}"
chmod 600 "${CRED}"
: > "${TARGET_FILE}"

# 起動待ち・追従待ちの上限（秒）。
START_TIMEOUT=30
CONVERGE_TIMEOUT=60
# 送信側の ack 待ちの上限（秒）と未確認量の上限（最小値）。停滞で確実に上限へ当てるため小さくする。
ACK_TIMEOUT=2
MAX_UNACKED=8192

sleep_ms() {
  sleep "$(printf '%d.%03d' $((${1} / 1000)) $((${1} % 1000)))"
}

rand_ms() { # lo hi
  echo $(($1 + RANDOM % ($2 - $1 + 1)))
}

# ログの連結（起動ごとに別ファイル。連番はゼロ詰めで辞書順 = 起動順）。
cat_logs() { # primary|replica
  cat "${WORKDIR}"/"$1"_*.log 2>/dev/null
}
count_pat() { # primary|replica pattern
  local n
  n="$(cat_logs "$1" | grep -c -- "$2")"
  echo "${n:-0}"
}

wait_pat() { # primary|replica pattern target-count timeout-secs what
  local waited=0 limit=$(($4 * 10))
  while [ "${waited}" -lt "${limit}" ]; do
    [ "$(count_pat "$1" "$2")" -ge "$3" ] && return 0
    sleep 0.1
    waited=$((waited + 1))
  done
  fail "timed out waiting for $5"
}

# --- プロセス起動 ----------------------------------------------------------

PRIMARY_N=0
PRIMARY_PORT=""
REPL_PORT=""
# プライマリを起動する。引数は追加フラグ（同期レプリケーションなど）。起動のたびにポートが変わるため、
# 中継先ファイルを原子的に差し替える。
start_primary() {
  PRIMARY_N=$((PRIMARY_N + 1))
  local log
  log="$(printf '%s/primary_%03d.log' "${WORKDIR}" "${PRIMARY_N}")"
  local extra_sock=()
  if [ "${PRIMARY_N}" -eq 1 ]; then
    extra_sock=(--backup-socket "${SOCK}")
  fi
  "${BIN}" --users "${USERS}" --db "${PRIMARY_DB}" --bind 127.0.0.1:0 \
    --ddl-allowed-users alice --change-log "${extra_sock[@]}" \
    --replication-bind 127.0.0.1:0 --replication-roles "${ROLES}" \
    --replication-allow 127.0.0.1 \
    --replication-ack-timeout-secs "${ACK_TIMEOUT}" \
    --replication-max-unacked-bytes "${MAX_UNACKED}" \
    "$@" > /dev/null 2> "${log}" &
  primary_pid=$!
  local waited=0
  PRIMARY_PORT=""
  REPL_PORT=""
  while [ "${waited}" -lt $((START_TIMEOUT * 10)) ]; do
    PRIMARY_PORT="$(sed -n 's/.*listening on 127\.0\.0\.1:\([0-9][0-9]*\).*/\1/p' "${log}" | head -1)"
    REPL_PORT="$(sed -n 's/.*replication endpoint bound to 127\.0\.0\.1:\([0-9][0-9]*\).*/\1/p' "${log}" | head -1)"
    if [ -n "${PRIMARY_PORT}" ] && [ -n "${REPL_PORT}" ]; then
      printf '%s' "${REPL_PORT}" > "${TARGET_FILE}.new" && mv "${TARGET_FILE}.new" "${TARGET_FILE}"
      return 0
    fi
    if ! kill -0 "${primary_pid}" 2>/dev/null; then
      wait "${primary_pid}" 2>/dev/null
      primary_pid=""
      fail "primary exited before listening"
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  fail "primary did not start listening"
}

kill_primary() {
  stop_pid "${primary_pid}"
  primary_pid=""
  # 中継先を空にして、再起動までの接続を即時に閉じさせる。
  : > "${TARGET_FILE}"
}

PROXY_PORT=""
start_proxy() {
  "${PROXY}" --listen 127.0.0.1:0 --target-file "${TARGET_FILE}" > "${WORKDIR}/proxy.log" 2>&1 &
  proxy_pid=$!
  local waited=0
  while [ "${waited}" -lt 100 ]; do
    PROXY_PORT="$(sed -n 's/^PROXY_PORT=\([0-9][0-9]*\)$/\1/p' "${WORKDIR}/proxy.log" | head -1)"
    [ -n "${PROXY_PORT}" ] && return 0
    sleep 0.1
    waited=$((waited + 1))
  done
  fail "proxy did not start"
}

REPLICA_N=0
REPLICA_PORT=""
start_replica() {
  REPLICA_N=$((REPLICA_N + 1))
  local log
  log="$(printf '%s/replica_%03d.log' "${WORKDIR}" "${REPLICA_N}")"
  "${BIN}" --users "${USERS}" --db "${REPLICA_DB}" --bind 127.0.0.1:0 \
    --replica-primary "127.0.0.1:${PROXY_PORT}" --replica-credentials "${CRED}" \
    --replica-max-backoff-secs 1 > /dev/null 2> "${log}" &
  replica_pid=$!
  local waited=0
  REPLICA_PORT=""
  while [ "${waited}" -lt $((START_TIMEOUT * 10)) ]; do
    REPLICA_PORT="$(sed -n 's/.*listening on 127\.0\.0\.1:\([0-9][0-9]*\).*/\1/p' "${log}" | head -1)"
    [ -n "${REPLICA_PORT}" ] && return 0
    if ! kill -0 "${replica_pid}" 2>/dev/null; then
      wait "${replica_pid}" 2>/dev/null
      replica_pid=""
      fail "replica exited before listening"
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  fail "replica did not start listening"
}

# レプリカが生きており、停止系（適用停止・プライマリの拒否・別 DB・プロトコル違反）の文言が出ていないこと。
assert_replica_alive() {
  kill -0 "${replica_pid}" 2>/dev/null || fail "replica process is gone ($1)"
  local pat
  for pat in "apply halted" "rejected the connection" "another database" "protocol violation"; do
    [ "$(count_pat replica "${pat}")" -eq 0 ] || fail "replica reported '${pat}' ($1)"
  done
}

# --- クライアント ----------------------------------------------------------

pw_of() { [ "$1" = "alice" ] && echo "${PW_ALICE}" || echo "${PW_BOB}"; }
cli() { # port user subcommand args...
  local port="$1" user="$2"
  shift 2
  python3 -I "${CLIENT}" "$1" "${port}" "${user}" "$(pw_of "${user}")" "${@:2}"
}
on_primary() { cli "${PRIMARY_PORT}" "$@"; }
on_replica() { cli "${REPLICA_PORT}" "$@"; }

# 書き込みクライアント（テナントごとに 1 本）。起動のたびに id の範囲を新しくする。
WRITER_N=0
WA_LOG=""
WB_LOG=""
EXTRA=""
start_writers() {
  WRITER_N=$((WRITER_N + 1))
  WA_LOG="${WORKDIR}/wa_${WRITER_N}.log"
  WB_LOG="${WORKDIR}/wb_${WRITER_N}.log"
  WA_BASE=$((WRITER_N * 1000000))
  WB_BASE=$((500000000 + WRITER_N * 1000000))
  # 関数経由だと PID がサブシェルのものになり python を kill できないため、直接起動する。
  python3 -I "${CLIENT}" churn "${PRIMARY_PORT}" alice "${PW_ALICE}" "${WA_BASE}" "${EXTRA}" > "${WA_LOG}" 2>/dev/null &
  wa_pid=$!
  python3 -I "${CLIENT}" churn "${PRIMARY_PORT}" bob "${PW_BOB}" "${WB_BASE}" "${EXTRA}" > "${WB_LOG}" 2>/dev/null &
  wb_pid=$!
}
stop_writers() {
  stop_pid "${wa_pid}"
  stop_pid "${wb_pid}"
  wa_pid=""
  wb_pid=""
}

acks_of() { # file
  local n
  n="$(grep -c '^ACK ' "$1" 2>/dev/null)"
  echo "${n:-0}"
}

# 書き込みの最中か（両 writer が生きていて、直近 300ms に ACK が進んだ）。
mid_write() {
  local a1 b1
  a1="$(acks_of "${WA_LOG}")"
  b1="$(acks_of "${WB_LOG}")"
  sleep 0.3
  kill -0 "${wa_pid}" 2>/dev/null || return 1
  kill -0 "${wb_pid}" 2>/dev/null || return 1
  [ "$(acks_of "${WA_LOG}")" -gt "${a1}" ] && [ "$(acks_of "${WB_LOG}")" -gt "${b1}" ]
}

# ACK を受けた書き込みが 1 本以上進むまで待つ。
wait_writes() { # min-acks
  local waited=0
  while [ "${waited}" -lt 300 ]; do
    [ "$(acks_of "${WA_LOG}")" -ge "$1" ] && [ "$(acks_of "${WB_LOG}")" -ge "$1" ] && return 0
    sleep 0.1
    waited=$((waited + 1))
  done
  fail "writers did not make progress"
}

# --- 比較 -------------------------------------------------------------------

TABLES="docs:id kv:k markers:id"
MARK_SEQ=0
fp() { # port user
  # shellcheck disable=SC2086
  cli "$1" "$2" fingerprint ${TABLES}
}

# 書き込みを止め、目印がレプリカから見えるのを待ち、テナントごとに全表を比べる。流れ込み中の commit が
# 目印より後に入る競合を避けるため、不一致なら新しい目印で最大 3 回やり直す（恒常的な差は残る）。
converge_and_compare() { # stage
  local stage="$1" attempt user a b rc
  stop_writers
  sleep 0.5
  assert_replica_alive "before compare, stage ${stage}"
  for attempt in 1 2 3; do
    MARK_SEQ=$((MARK_SEQ + 1))
    on_primary alice marker $((MARK_SEQ * 2)) > /dev/null || fail "marker (alice) failed"
    on_primary bob marker $((MARK_SEQ * 2 + 1)) > /dev/null || fail "marker (bob) failed"
    on_replica alice wait-marker $((MARK_SEQ * 2)) "${CONVERGE_TIMEOUT}" > /dev/null \
      || fail "replica did not converge (stage ${stage}, alice marker)"
    on_replica bob wait-marker $((MARK_SEQ * 2 + 1)) "${CONVERGE_TIMEOUT}" > /dev/null \
      || fail "replica did not converge (stage ${stage}, bob marker)"
    rc=0
    for user in alice bob; do
      a="$(fp "${PRIMARY_PORT}" "${user}")" || fail "fingerprint on primary failed"
      b="$(fp "${REPLICA_PORT}" "${user}")" || fail "fingerprint on replica failed"
      if printf '%s\n' "${a}" | grep -q ' missing$'; then
        fail "a table is missing on the primary (stage ${stage}, ${user}); the comparison would be vacuous"
      fi
      if [ "${a}" != "${b}" ]; then
        rc=1
        echo "stage ${stage} attempt ${attempt}: mismatch for ${user}" >&2
        diff <(printf '%s\n' "${a}") <(printf '%s\n' "${b}") >&2
      fi
    done
    if [ "${rc}" -eq 0 ]; then
      assert_replica_alive "after compare, stage ${stage}"
      echo "stage ${stage}: replica matches primary ($(printf '%s\n' "${a}" | tr '\n' ' ' | sed 's/sha256=[0-9a-f]*//g'))"
      return 0
    fi
    sleep 1
  done
  fail "replica content differs from primary (stage ${stage})"
}

# =============================================================================
# 段 0: 準備
# =============================================================================
start_primary
start_proxy
on_primary alice rsetup > /dev/null || fail "rsetup failed"
on_primary alice rseed 0 > /dev/null || fail "seed (alice) failed"
on_primary bob rseed 500000000 > /dev/null || fail "seed (bob) failed"
"${BIN}" backup --socket "${SOCK}" --out "${REPLICA_DB}" > /dev/null 2>&1 || fail "backup failed"
rc=0
"${BIN}" verify --db "${REPLICA_DB}" > /dev/null 2>&1 || rc=$?
[ "${rc}" -eq 0 ] || fail "base verify exit ${rc}"
# ベースを取った後にも書き込み、ベース以降の差分が必ずある状態にする。
on_primary alice marker 900000 > /dev/null || fail "post-base marker failed"
start_replica
wait_pat replica 'connected (applied lsn=' 1 "${START_TIMEOUT}" "replica to connect"
start_writers
wait_writes 5
converge_and_compare 0

# =============================================================================
# 段 1: レプリカの SIGKILL（書き込み中）
# =============================================================================
hits=0
i=0
while [ "${i}" -lt "${REPLICA_KILLS}" ] || { [ "${hits}" -lt 1 ] && [ "${i}" -lt $((REPLICA_KILLS * 4)) ]; }; do
  i=$((i + 1))
  start_writers
  wait_writes 3
  sleep_ms "$(rand_ms 300 800)"
  if mid_write; then hits=$((hits + 1)); fi
  stop_pid "${replica_pid}"
  replica_pid=""
  rc=0
  "${BIN}" verify --db "${REPLICA_DB}" > /dev/null 2>&1 || rc=$?
  [ "${rc}" -eq 0 ] || fail "replica db verify exit ${rc} after SIGKILL (cycle ${i})"
  before="$(count_pat replica 'connected (applied lsn=')"
  start_replica
  wait_pat replica 'connected (applied lsn=' $((before + 1)) "${START_TIMEOUT}" "replica to reconnect after kill"
  stop_writers
done
[ "${hits}" -ge 1 ] || fail "no replica kill landed during writes"
# 再接続の適用位置が単調非減少（先頭からやり直していない）。
prev=0
for n in $(cat_logs replica | sed -n 's/.*connected (applied lsn=\([0-9]*\)).*/\1/p'); do
  [ "${n}" -ge "${prev}" ] || fail "applied lsn went backwards on reconnect (${prev} -> ${n})"
  prev="${n}"
done
start_writers
wait_writes 3
converge_and_compare 1
echo "stage 1: ${i} replica kills (${hits} during writes)"

# =============================================================================
# 段 2: 中継の接続断（両プロセスは生存）
# =============================================================================
start_writers
wait_writes 3
base_disc="$(count_pat replica 'disconnected, will reconnect')"
for ((i = 1; i <= CUTS; i++)); do
  before="$(count_pat replica 'connected (applied lsn=')"
  sleep_ms "$(rand_ms 200 600)"
  kill -USR1 "${proxy_pid}" || fail "proxy signal failed"
  wait_pat replica 'connected (applied lsn=' $((before + 1)) "${START_TIMEOUT}" "replica to reconnect after cut"
done
[ "$(count_pat replica 'disconnected, will reconnect')" -ge $((base_disc + CUTS)) ] \
  || fail "replica did not observe the injected disconnects"
kill -0 "${replica_pid}" 2>/dev/null || fail "replica process died during cuts"
converge_and_compare 2
echo "stage 2: ${CUTS} connection cuts"

# =============================================================================
# 段 3: 遅延（ack 待ちの上限）
# =============================================================================
base_to="$(count_pat primary 'unacknowledged data timed out')"
start_writers
wait_writes 3
before="$(count_pat replica 'connected (applied lsn=')"
kill -USR2 "${proxy_pid}" || fail "proxy signal failed"
sleep $((ACK_TIMEOUT + 3))
kill -USR2 "${proxy_pid}" || fail "proxy signal failed"
wait_pat replica 'connected (applied lsn=' $((before + 1)) "${START_TIMEOUT}" "replica to reconnect after stall"
[ "$(count_pat primary 'unacknowledged data timed out')" -gt "${base_to}" ] \
  || fail "the ack timeout was not triggered by the proxy stall"
# 続けてレプリカ自体の停止（SIGSTOP → SIGCONT）。
base_to="$(count_pat primary 'unacknowledged data timed out')"
before="$(count_pat replica 'connected (applied lsn=')"
wait_writes 3
kill -STOP "${replica_pid}" || fail "SIGSTOP failed"
sleep $((ACK_TIMEOUT + 3))
kill -CONT "${replica_pid}" || fail "SIGCONT failed"
wait_pat replica 'connected (applied lsn=' $((before + 1)) "${START_TIMEOUT}" "replica to reconnect after SIGSTOP"
[ "$(count_pat primary 'unacknowledged data timed out')" -gt "${base_to}" ] \
  || fail "the ack timeout was not triggered by the replica stop"
converge_and_compare 3
echo "stage 3: stalls observed as ack timeouts"

# =============================================================================
# 段 4: DDL を挟む
# =============================================================================
start_writers
wait_writes 3
on_primary alice ddl 1 > /dev/null || fail "ddl failed"
TABLES="docs:id kv:k markers:id late:id"
# 以後の書き込みは追加した列も使う（旧 writer は止めて新しく起動する）。
stop_writers
EXTRA="extra"
start_writers
wait_writes 5
converge_and_compare 4
echo "stage 4: DDL replicated"

# =============================================================================
# 段 5: プライマリの SIGKILL と再起動
# =============================================================================
hits=0
i=0
while [ "${i}" -lt "${PRIMARY_KILLS}" ] || { [ "${hits}" -lt 1 ] && [ "${i}" -lt $((PRIMARY_KILLS * 4)) ]; }; do
  i=$((i + 1))
  start_writers
  wait_writes 3
  sleep_ms "$(rand_ms 300 800)"
  if mid_write; then hits=$((hits + 1)); fi
  kill_primary
  stop_writers
  last_a=$(($(acks_of "${WA_LOG}") - 1))
  last_b=$(($(acks_of "${WB_LOG}") - 1))
  sleep 0.3
  before="$(count_pat replica 'connected (applied lsn=')"
  start_primary
  wait_pat replica 'connected (applied lsn=' $((before + 1)) "${START_TIMEOUT}" "replica to reconnect after primary restart"
  assert_replica_alive "after primary restart ${i}"
  # ACK を受けた最後の commit が、再起動後のプライマリに残っている。
  if [ "${last_a}" -ge 0 ]; then
    on_primary alice ids-present docs $((WA_BASE + last_a)) $((WA_BASE + last_a)) > /dev/null \
      || fail "an acknowledged commit was lost after primary restart (cycle ${i})"
  fi
done
[ "${hits}" -ge 1 ] || fail "no primary kill landed during writes"
start_writers
wait_writes 3
converge_and_compare 5
echo "stage 5: ${i} primary kills (${hits} during writes)"

# =============================================================================
# 段 6: 同期レプリケーションとプライマリの喪失
# =============================================================================
stop_writers
kill_primary
before="$(count_pat replica 'connected (applied lsn=')"
start_primary --replication-sync-standby replica-1 --replication-sync-commit on
wait_pat replica 'connected (applied lsn=' $((before + 1)) "${START_TIMEOUT}" "replica to reconnect (sync mode)"
on_primary alice ddl 2 > /dev/null || fail "ddl 2 failed"
TABLES="docs:id kv:k markers:id late:id synclog:id"
SYNC_LOG="${WORKDIR}/sync.log"
python3 -I "${CLIENT}" sync-append "${PRIMARY_PORT}" alice "${PW_ALICE}" 1 > "${SYNC_LOG}" 2>/dev/null &
sw_pid=$!
waited=0
while [ "$(acks_of "${SYNC_LOG}")" -lt 50 ] && [ "${waited}" -lt 300 ]; do
  sleep 0.1
  waited=$((waited + 1))
done
[ "$(acks_of "${SYNC_LOG}")" -ge 50 ] || fail "sync writer did not make progress"
sleep_ms "$(rand_ms 100 400)"
kill_primary
stop_pid "${sw_pid}"
sw_pid=""
last_sync="$(grep '^ACK ' "${SYNC_LOG}" | tail -1 | awk '{print $2}')"
[[ "${last_sync}" =~ ^[0-9]+$ ]] || fail "no acknowledged sync commit"
# プライマリが止まっている間に、ACK を受けた全 id がレプリカにある（同期の性質）。
on_replica alice ids-present synclog 1 "${last_sync}" > /dev/null \
  || fail "an acknowledged synchronous commit is missing on the replica"
assert_replica_alive "after sync primary loss"
before="$(count_pat replica 'connected (applied lsn=')"
start_primary
wait_pat replica 'connected (applied lsn=' $((before + 1)) "${START_TIMEOUT}" "replica to reconnect after sync stage"
start_writers
wait_writes 3
converge_and_compare 6
echo "stage 6: ${last_sync} synchronous commits survived primary loss"

# =============================================================================
# 段 7: 終了時の検査
# =============================================================================
stop_writers
stop_pid "${replica_pid}"
replica_pid=""
stop_pid "${primary_pid}"
primary_pid=""
for db in "${PRIMARY_DB}" "${REPLICA_DB}"; do
  rc=0
  "${BIN}" verify --db "${db}" > "${WORKDIR}/verify.out" 2>&1 || rc=$?
  [ "${rc}" -eq 0 ] || fail "final verify exit ${rc} for $(basename "${db}")"
  echo "final verify ok: $(basename "${db}") $(grep '^rows_scanned=' "${WORKDIR}/verify.out")"
done

echo "OK: replication fault-injection checks passed"
