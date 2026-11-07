#!/usr/bin/env bash
# Issue #2157（RECOVER-13 ポインタ。親 #2154）のバックアップ取得・復元の crash 耐性回帰テスト。
#
# wire-server の `backup --db`（Issue #2152）・`verify --db`・`restore --from --db`（Issue #2156）を
# プロセス外から SIGKILL し、公開名（最終名）に不完全なファイルが現れないことと、完成した
# バックアップ・復元先が整合検査（終了コード 0）に通りサーバーで同じデータを読めることを確かめる。
# `scripts/crash_test.sh` 系（engine 直叩きの Storage）と違い、運用者が使う CLI 経路そのものを
# 対象にするため、wire-server のリリースバイナリと最小 pg wire クライアント
# （scripts/crash_test_backup_restore_client.py。標準ライブラリのみ）を使う。
#
# 手順:
#   1. サーバーでデータを投入し、追記書き込みの最中にサーバーを SIGKILL する（書き込みのある
#      DB が異常終了した状態を作る。追記クライアントが受けた確認済み最大 id を記録）。
#   2. 基準の `backup`（無中断）を 1 回実行して所要時間を測り、整合・件数を確かめる。
#   3. 取得中 kill: 基準時間の 0〜1.5 倍のランダム待機後に `backup` を SIGKILL する反復。
#      最終名は「存在しない」か「存在して verify が通り基準と同じ件数」のどちらかで、
#      元 DB も verify が通ること、残骸は一時名（`.fandhe-snapshot-*.tmp`）だけであることを検査する。
#   4. 復元: 完成したバックアップを `restore` し、verify 0・サーバーで件数一致を確かめる。
#   5. 復元中 kill: 手順 3 と同形（一時名は `.fandhe-restore-*.tmp`）。
#   6. 稼働中取得中のサーバー kill: `--backup-socket` 経由の取得中にサーバーを SIGKILL し、
#      最終名が不完全にならず、同じ DB でサーバーを再起動して件数が保たれることを確かめる。
# 空虚な成功を避けるため、kill が実際に実行中のプロセスへ当たった回数（終了ステータス 137）を
# 数え、0 回なら失敗とする。Makefile の crash-test-backup-restore ターゲット（crash-test の
# 前提）から呼ばれ、CI では既存の crash-test ジョブで実行される。
#
# 使い方: scripts/crash_test_backup_restore.sh [反復回数（既定 12）]

set -u

# `kill -9` は 137、`wait` はシグナル終了したプロセスの終了コードを返すため、set -e は
# 相性が悪い。検証結果は都度 fail-closed に明示チェックする（crash_test.sh と同方針）。

ITERATIONS="${1:-12}"
if ! [[ "${ITERATIONS}" =~ ^[0-9]+$ ]] || [ "${ITERATIONS}" -lt 1 ]; then
  echo "ERROR: iterations must be a positive integer, got: ${ITERATIONS}" >&2
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

# バックグラウンドの子（サーバー・追記クライアント・取得/復元の CLI）の PID。trap 設置前に
# 初期化する（crash_test.sh の writer_pid と同じ理由。set -u 下で cleanup が未定義参照しない）。
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
BAK_DIR="${WORKDIR}/bak"       # backup の出力先（残骸検査の対象）
RST_DIR="${WORKDIR}/rst"       # restore の復元先（残骸検査の対象）
LIVE_DIR="${WORKDIR}/live"     # 稼働中取得（ソケット）用。親ディレクトリは 0700 が必須
mkdir "${SRC_DIR}" "${BAK_DIR}" "${RST_DIR}" "${LIVE_DIR}"
chmod 700 "${LIVE_DIR}"

USER_NAME="alice"
USER_PASS="pw-crash-test"
USERS_FILE="${SRC_DIR}/users.txt"
SERVER_LOG="${SRC_DIR}/server.log"
CLIENT_LOG="${SRC_DIR}/client.log"

# 初期ロール用のユーザーストア（ダミーのテスト用パスワード。argon2id の PHC 文字列を生成する）。
phc="$(printf '%s\n' "${USER_PASS}" | "${BIN}" hash-password)" || fail "hash-password failed"
printf '%s:tenant-a:%s\n' "${USER_NAME}" "${phc}" > "${USERS_FILE}"

# 投入行数（1 行約 1KB）。取得に数十〜数百 ms かかる程度の大きさにして kill の当たり所を作る。
SEED_ROWS=20000
# サーバー起動待ち・追記開始待ちの上限（100ms 単位。300 = 30 秒）。
START_TIMEOUT_TENTHS=300

# 引数のミリ秒を `sleep` が受ける小数秒へ変換して待つ。
sleep_ms() {
  local ms="$1"
  sleep "$(printf '%d.%03d' $((ms / 1000)) $((ms % 1000)))"
}

now_ms() {
  echo $(($(date +%s%N) / 1000000))
}

# サーバーを起動する。引数は `<db パス> [追加オプション...]`。成功すると server_pid と
# SERVER_PORT が設定される（ポートは標準エラーの `listening on` 行から取る。0 番 bind）。
SERVER_PORT=""
start_server() {
  local db="$1"
  shift
  : > "${SERVER_LOG}"
  "${BIN}" --users "${USERS_FILE}" --db "${db}" --bind 127.0.0.1:0 \
    --ddl-allowed-users "${USER_NAME}" "$@" >> "${SERVER_LOG}" 2>&1 &
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

# `verify --db` を実行して終了コードと出力を VERIFY_RC / VERIFY_OUT へ設定する。
VERIFY_RC=0
VERIFY_OUT=""
run_verify() {
  VERIFY_OUT="$("${BIN}" verify --db "$1" 2>&1)"
  VERIFY_RC=$?
}

# 検査済みの整合 DB が基準と同じ件数であることを、verify の rows_scanned で確かめる。
expect_verify_ok() {
  local path="$1" what="$2"
  run_verify "${path}"
  if [ "${VERIFY_RC}" -ne 0 ]; then
    echo "${VERIFY_OUT}" >&2
    fail "verify exit ${VERIFY_RC} (expected 0) for ${what}: ${path}"
  fi
  local rows
  rows="$(printf '%s\n' "${VERIFY_OUT}" | sed -n 's/^rows_scanned=\([0-9]*\)$/\1/p')"
  if [ "${rows}" != "${BASE_ROWS_SCANNED}" ]; then
    echo "${VERIFY_OUT}" >&2
    fail "rows_scanned=${rows} differs from baseline ${BASE_ROWS_SCANNED} for ${what}: ${path}"
  fi
}

# 出力ディレクトリの残骸検査。許可するのは、完成済みの最終名（`keep-*` 以外の `snap-*`/`rst-*`）と
# 一時名（引数の glob 接頭辞 `.fandhe-snapshot-` / `.fandhe-restore-` + `.tmp` 終端）だけ。
# 一時名は 0600 であること（全テナントのデータのコピーのため）も確かめる。
check_residue() {
  local dir="$1" tmp_prefix="$2" final_prefix="$3" entry name mode
  for entry in "${dir}"/* "${dir}"/.[!.]*; do
    [ -e "${entry}" ] || continue
    name="$(basename "${entry}")"
    case "${name}" in
      "${tmp_prefix}"*.tmp)
        mode="$(stat -c '%a' "${entry}")"
        [ "${mode}" = "600" ] || fail "temp residue ${entry} has mode ${mode}, expected 600"
        ;;
      "${final_prefix}"* | backup.sock) ;;
      *) fail "unexpected entry in ${dir}: ${name}" ;;
    esac
  done
}

# ---------------------------------------------------------------------------
# 1. データ投入 → 追記書き込み中にサーバーを SIGKILL（書き込みのある DB の異常終了）
# ---------------------------------------------------------------------------
SRC_DB="${SRC_DIR}/source.redb"
echo "=== phase 1: seed ${SEED_ROWS} rows, then SIGKILL the server during appends ==="
start_server "${SRC_DB}"
python3 -I "${CLIENT}" seed "${SERVER_PORT}" "${USER_NAME}" "${USER_PASS}" "${SEED_ROWS}" \
  || fail "seed failed"

APPEND_START_ID=1000000
: > "${CLIENT_LOG}"
python3 -I "${CLIENT}" append "${SERVER_PORT}" "${USER_NAME}" "${USER_PASS}" "${APPEND_START_ID}" \
  >> "${CLIENT_LOG}" 2>&1 &
client_pid=$!
waited=0
while ! grep -q '^ACK ' "${CLIENT_LOG}" 2>/dev/null; do
  if [ "${waited}" -ge "${START_TIMEOUT_TENTHS}" ] || ! kill -0 "${client_pid}" 2>/dev/null; then
    cat "${CLIENT_LOG}" >&2
    fail "append client did not report an acknowledged insert"
  fi
  sleep 0.1
  waited=$((waited + 1))
done
# 追記を少し進めてから kill する（サーバー kill 時にコミット中の書き込みがある確率を上げる）。
sleep_ms $((600 + RANDOM % 600))
stop_server
stop_pid "${client_pid}"
client_pid=""

LAST_ACK="$(sed -n 's/^ACK \([0-9]*\)$/\1/p' "${CLIENT_LOG}" | tail -1)"
[[ "${LAST_ACK}" =~ ^[0-9]+$ ]] || fail "no acknowledged append before the server kill"
ACKED_APPENDS=$((LAST_ACK - APPEND_START_ID + 1))
echo "last acknowledged append id=${LAST_ACK} (acked appends=${ACKED_APPENDS})"

# ---------------------------------------------------------------------------
# 2. 基準の backup（無中断）
# ---------------------------------------------------------------------------
echo "=== phase 2: baseline backup ==="
BASE_BAK="${BAK_DIR}/snap-base.redb"
t0="$(now_ms)"
"${BIN}" backup --db "${SRC_DB}" --out "${BASE_BAK}"
rc=$?
BACKUP_MS=$(($(now_ms) - t0))
[ "${rc}" -eq 0 ] || fail "baseline backup exit ${rc} (expected 0)"
[ -f "${BASE_BAK}" ] || fail "baseline backup left no output"
echo "baseline backup took ${BACKUP_MS}ms"

run_verify "${BASE_BAK}"
[ "${VERIFY_RC}" -eq 0 ] || { echo "${VERIFY_OUT}" >&2; fail "baseline verify exit ${VERIFY_RC}"; }
BASE_ROWS_SCANNED="$(printf '%s\n' "${VERIFY_OUT}" | sed -n 's/^rows_scanned=\([0-9]*\)$/\1/p')"
[[ "${BASE_ROWS_SCANNED}" =~ ^[0-9]+$ ]] || fail "could not parse rows_scanned"
run_verify "${SRC_DB}"
[ "${VERIFY_RC}" -eq 0 ] || { echo "${VERIFY_OUT}" >&2; fail "source verify exit ${VERIFY_RC}"; }

# 基準の件数。確認済みの追記が失われていない・id に欠番が無いことを元 DB 側で押さえる
# （追記は 1 行ずつ確認するため、コミット済みで確認前の 1 行だけが多くなりうる）。
count_via_server "${BASE_BAK}"
BASE_COUNT="${COUNT}"
BASE_MAXID="${MAXID}"
MIN_COUNT=$((SEED_ROWS + ACKED_APPENDS))
if [ "${BASE_COUNT}" -lt "${MIN_COUNT}" ] || [ "${BASE_COUNT}" -gt $((MIN_COUNT + 1)) ]; then
  fail "baseline count ${BASE_COUNT} is outside [${MIN_COUNT}, $((MIN_COUNT + 1))] (data loss or phantom rows)"
fi
if [ "${BASE_MAXID}" -lt "${LAST_ACK}" ]; then
  fail "baseline max id ${BASE_MAXID} is below the last acknowledged id ${LAST_ACK}"
fi
echo "baseline: count=${BASE_COUNT} max_id=${BASE_MAXID} rows_scanned=${BASE_ROWS_SCANNED}"

# ---------------------------------------------------------------------------
# 3. backup 取得中の SIGKILL
# ---------------------------------------------------------------------------
# 待機の上限（ms）。基準時間の 1.5 倍（最低 20ms）。
backup_limit_ms=$((BACKUP_MS * 3 / 2))
[ "${backup_limit_ms}" -ge 20 ] || backup_limit_ms=20
echo "=== phase 3: SIGKILL during backup (${ITERATIONS} iterations, delay 0-${backup_limit_ms}ms) ==="
backup_killed=0
backup_completed=0
for i in $(seq 1 "${ITERATIONS}"); do
  out="${BAK_DIR}/snap-${i}.redb"
  "${BIN}" backup --db "${SRC_DB}" --out "${out}" >/dev/null 2>&1 &
  job_pid=$!
  sleep_ms $((RANDOM % (backup_limit_ms + 1)))
  kill -9 "${job_pid}" 2>/dev/null
  wait "${job_pid}" 2>/dev/null
  status=$?
  job_pid=""
  if [ "${status}" -eq 137 ]; then
    backup_killed=$((backup_killed + 1))
  else
    # kill より先に終了していた（完了）。成功終了のはず（3 は後始末不確実で完成済み）。
    case "${status}" in
      0 | 3) backup_completed=$((backup_completed + 1)) ;;
      *) fail "backup iteration ${i} exited with ${status} (expected 0, 3 or 137)" ;;
    esac
  fi
  if [ -e "${out}" ]; then
    # 最終名が存在するなら完全でなければならない。
    expect_verify_ok "${out}" "backup output of iteration ${i}"
  fi
  expect_verify_ok "${SRC_DB}" "source DB after backup iteration ${i}"
  check_residue "${BAK_DIR}" ".fandhe-snapshot-" "snap-"
done
echo "backup kills: killed=${backup_killed} completed_before_kill=${backup_completed}"
if [ "${backup_killed}" -lt 1 ]; then
  fail "no backup was killed mid-run (the kill timing never hit; test is vacuous)"
fi
# 一時名の残骸は運用者が手動で消す前提（README）。ここで数えて報告し、後続のためにファイルは残す。
backup_residue="$(find "${BAK_DIR}" -maxdepth 1 -name '.fandhe-snapshot-*.tmp' | wc -l)"
echo "backup temp residue files: ${backup_residue}"

# ---------------------------------------------------------------------------
# 4. 完成したバックアップからの復元と、復元先でのサーバー起動
# ---------------------------------------------------------------------------
echo "=== phase 4: restore from the completed backup ==="
RST_BASE="${RST_DIR}/rst-base.redb"
t0="$(now_ms)"
"${BIN}" restore --from "${BASE_BAK}" --db "${RST_BASE}"
rc=$?
RESTORE_MS=$(($(now_ms) - t0))
[ "${rc}" -eq 0 ] || fail "restore exit ${rc} (expected 0)"
echo "baseline restore took ${RESTORE_MS}ms"
expect_verify_ok "${RST_BASE}" "restored database"
count_via_server "${RST_BASE}"
if [ "${COUNT}" != "${BASE_COUNT}" ] || [ "${MAXID}" != "${BASE_MAXID}" ]; then
  fail "restored data differs: count=${COUNT} max_id=${MAXID}, expected ${BASE_COUNT}/${BASE_MAXID}"
fi
echo "restored: count=${COUNT} max_id=${MAXID} (server read OK)"

# ---------------------------------------------------------------------------
# 5. restore 中の SIGKILL
# ---------------------------------------------------------------------------
restore_limit_ms=$((RESTORE_MS * 3 / 2))
[ "${restore_limit_ms}" -ge 20 ] || restore_limit_ms=20
echo "=== phase 5: SIGKILL during restore (${ITERATIONS} iterations, delay 0-${restore_limit_ms}ms) ==="
restore_killed=0
restore_completed=0
for i in $(seq 1 "${ITERATIONS}"); do
  out="${RST_DIR}/rst-${i}.redb"
  "${BIN}" restore --from "${BASE_BAK}" --db "${out}" >/dev/null 2>&1 &
  job_pid=$!
  sleep_ms $((RANDOM % (restore_limit_ms + 1)))
  kill -9 "${job_pid}" 2>/dev/null
  wait "${job_pid}" 2>/dev/null
  status=$?
  job_pid=""
  if [ "${status}" -eq 137 ]; then
    restore_killed=$((restore_killed + 1))
  else
    case "${status}" in
      0 | 3) restore_completed=$((restore_completed + 1)) ;;
      *) fail "restore iteration ${i} exited with ${status} (expected 0, 3 or 137)" ;;
    esac
  fi
  if [ -e "${out}" ]; then
    expect_verify_ok "${out}" "restore output of iteration ${i}"
  fi
  # 復元元のバックアップは読み取りだけで変わらない。
  expect_verify_ok "${BASE_BAK}" "restore source after iteration ${i}"
  check_residue "${RST_DIR}" ".fandhe-restore-" "rst-"
done
echo "restore kills: killed=${restore_killed} completed_before_kill=${restore_completed}"
if [ "${restore_killed}" -lt 1 ]; then
  fail "no restore was killed mid-run (the kill timing never hit; test is vacuous)"
fi
restore_residue="$(find "${RST_DIR}" -maxdepth 1 -name '.fandhe-restore-*.tmp' | wc -l)"
echo "restore temp residue files: ${restore_residue}"

# 殺された restore の後でも、同じ復元先名で取り直せ、サーバーで読める。
RST_RETRY="${RST_DIR}/rst-retry.redb"
"${BIN}" restore --from "${BASE_BAK}" --db "${RST_RETRY}" >/dev/null \
  || fail "restore retry after killed restores failed"
expect_verify_ok "${RST_RETRY}" "restore retry"

# ---------------------------------------------------------------------------
# 6. 稼働中取得（--backup-socket）の最中にサーバーを SIGKILL
# ---------------------------------------------------------------------------
LIVE_ITERATIONS="${ITERATIONS}"
LIVE_DB="${LIVE_DIR}/live.redb"
LIVE_SOCK="${LIVE_DIR}/backup.sock"
"${BIN}" restore --from "${BASE_BAK}" --db "${LIVE_DB}" >/dev/null || fail "live DB restore failed"
# 稼働中取得の所要時間の基準（無中断）。
start_server "${LIVE_DB}" --backup-socket "${LIVE_SOCK}"
t0="$(now_ms)"
"${BIN}" backup --socket "${LIVE_SOCK}" --out "${LIVE_DIR}/live-base.redb" >/dev/null 2>&1
rc=$?
LIVE_MS=$(($(now_ms) - t0))
[ "${rc}" -eq 0 ] || fail "baseline live backup exit ${rc} (expected 0)"
expect_verify_ok "${LIVE_DIR}/live-base.redb" "baseline live backup"
stop_server
# 稼働中取得は基準時間の範囲内で待つ（完了後の kill を減らし、取得中に当てる確率を上げる）。
live_limit_ms="${LIVE_MS}"
[ "${live_limit_ms}" -ge 20 ] || live_limit_ms=20
echo "=== phase 6: SIGKILL the server during a live backup (${LIVE_ITERATIONS} iterations, delay 0-${live_limit_ms}ms) ==="
live_killed=0
live_interrupted=0
for i in $(seq 1 "${LIVE_ITERATIONS}"); do
  out="${LIVE_DIR}/live-${i}.redb"
  start_server "${LIVE_DB}" --backup-socket "${LIVE_SOCK}"
  "${BIN}" backup --socket "${LIVE_SOCK}" --out "${out}" >/dev/null 2>&1 &
  job_pid=$!
  sleep_ms $((RANDOM % (live_limit_ms + 1)))
  if kill -9 "${server_pid}" 2>/dev/null; then
    live_killed=$((live_killed + 1))
  fi
  wait "${server_pid}" 2>/dev/null
  server_pid=""
  # CLI はサーバー消失で失敗終了するか、既に完了している。いずれも終了を待つ（タイムアウト付き）。
  waited=0
  while kill -0 "${job_pid}" 2>/dev/null; do
    if [ "${waited}" -ge "${START_TIMEOUT_TENTHS}" ]; then
      fail "live backup CLI did not exit after the server was killed (iteration ${i})"
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  wait "${job_pid}" 2>/dev/null
  cli_status=$?
  job_pid=""
  # CLI が失敗終了した（出力も無い）回数 = 取得中にサーバーが消えた回数。
  if [ "${cli_status}" -ne 0 ] && [ ! -e "${out}" ]; then
    live_interrupted=$((live_interrupted + 1))
  fi
  if [ -e "${out}" ]; then
    expect_verify_ok "${out}" "live backup output of iteration ${i}"
  fi
  # 殺されたサーバーの DB は、次の起動（修復を含む）で件数が保たれる。
  expect_verify_ok "${LIVE_DB}" "live DB after server kill ${i}"
  check_residue "${LIVE_DIR}" ".fandhe-snapshot-" "live"
done
if [ "${live_interrupted}" -lt 1 ]; then
  fail "no live backup was interrupted by the server kill (the kill timing never hit; test is vacuous)"
fi
count_via_server "${LIVE_DB}"
if [ "${COUNT}" != "${BASE_COUNT}" ] || [ "${MAXID}" != "${BASE_MAXID}" ]; then
  fail "live DB data changed: count=${COUNT} max_id=${MAXID}, expected ${BASE_COUNT}/${BASE_MAXID}"
fi
echo "live backup: server killed ${live_killed}/${LIVE_ITERATIONS} times, interrupted=${live_interrupted}, data intact"

echo "crash_test_backup_restore.sh: OK (backup kills=${backup_killed}, restore kills=${restore_killed}, live kills=${live_killed}; baseline backup=${BACKUP_MS}ms restore=${RESTORE_MS}ms)"
