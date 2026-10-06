#!/usr/bin/env bash
# Issue #1329: scripts/check_worktree_lag.sh のセルフテスト。
#
# mktemp -d 配下に bare origin と clone を作り、遅れ・競合・ahead・submodule などの
# シナリオを再現して、判定結果と「読み取り専用であること」（実行前後で status / HEAD /
# refs / stash が不変）を検証する。削除対象は本スクリプトが作る一時ディレクトリのみ。
# Makefile の worktree-lag-check-selftest ターゲットから呼ばれる。

set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/check_worktree_lag.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

GITC=(git -c user.name=test -c user.email=test@example.invalid -c protocol.file.allow=always -c init.defaultBranch=main)
FAILS=0
ok() { echo "ok: $*"; }
ng() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
expect() { # expect <desc> <haystack> <needle>
  if printf '%s\n' "$2" | grep -Fq -- "$3"; then ok "$1"; else ng "$1 (missing: $3)"; fi
}
expect_rc() { if [ "${RC}" -eq "$2" ]; then ok "$1"; else ng "$1 (exit ${RC})"; fi; }
refute() {
  if printf '%s\n' "$2" | grep -Fq -- "$3"; then ng "$1 (found: $3)"; else ok "$1"; fi
}

# 読み取り専用性の指紋（status / HEAD / refs / stash）
fingerprint() {
  {
    "${GITC[@]}" -C "$1" status --porcelain
    "${GITC[@]}" -C "$1" rev-parse HEAD
    "${GITC[@]}" -C "$1" for-each-ref
    "${GITC[@]}" -C "$1" stash list
  } | sha256sum
}

# scenario <name>: origin(bare) + 作業 clone(c) + 別 clone(up) を作り、origin/main へ 2 コミット進める準備をする
setup() {
  local d="${TMP}/$1"
  mkdir -p "${d}"
  "${GITC[@]}" init -q --bare "${d}/origin.git"
  "${GITC[@]}" clone -q "${d}/origin.git" "${d}/c" 2>/dev/null
  "${GITC[@]}" clone -q "${d}/origin.git" "${d}/up" 2>/dev/null
  echo base >"${d}/c/a.txt"
  echo lock0 >"${d}/c/Cargo.lock"
  "${GITC[@]}" -C "${d}/c" add -A
  "${GITC[@]}" -C "${d}/c" commit -q -m init
  "${GITC[@]}" -C "${d}/c" push -q origin HEAD:main
  "${GITC[@]}" -C "${d}/up" pull -q origin main 2>/dev/null
  echo "${d}"
}
upstream_commit() { # upstream_commit <d> <file> <content> : up で更新して push、c で fetch
  local d="$1"
  echo "$3" >"${d}/up/$2"
  "${GITC[@]}" -C "${d}/up" add -A
  "${GITC[@]}" -C "${d}/up" commit -q -m "up $2"
  "${GITC[@]}" -C "${d}/up" push -q origin HEAD:main
  "${GITC[@]}" -C "${d}/c" fetch -q origin
}
run() { # run <d> [ENV=VAL ...]: 出力を OUT、終了コードを RC へ
  local d="$1"; shift
  local before after
  before="$(fingerprint "${d}/c")"
  set +e
  OUT="$(env "$@" REPO="${d}/c" "${SCRIPT}" 2>&1)"
  RC=$?
  set -e
  after="$(fingerprint "${d}/c")"
  if [ "${before}" = "${after}" ]; then ok "read-only (state unchanged)"; else ng "state changed by the script"; fi
}

# S1: behind のみ・ローカル変更なし
d="$(setup s1)"; upstream_commit "${d}" a.txt new; upstream_commit "${d}" b.txt b
run "${d}" STRICT=1
expect "S1 behind=2" "${OUT}" "behind: 2, ahead: 0"
expect "S1 fast-forward possible" "${OUT}" "fast-forward: possible"
expect "S1 HIGH=0" "${OUT}" "HIGH=0"
expect_rc "S1 exit 0" 0

# S2: 取り込む側と同じファイルを未コミットで変更
d="$(setup s2)"; upstream_commit "${d}" a.txt new
echo local >"${d}/c/a.txt"
run "${d}" STRICT=1
expect "S2 HIGH reported" "${OUT}" "locally modified file also changed upstream: a.txt"
expect_rc "S2 STRICT exit 1" 1

# S3: 取り込む側が追加するパスに untracked が衝突
d="$(setup s3)"; upstream_commit "${d}" new.txt x
echo mine >"${d}/c/new.txt"
run "${d}"
expect "S3 collision HIGH" "${OUT}" "untracked file collides with a path added upstream (pull would abort): new.txt"
expect_rc "S3 non-strict exit 0" 0

# S4: ローカルが ahead
d="$(setup s4)"; upstream_commit "${d}" b.txt b
echo l >"${d}/c/l.txt"
"${GITC[@]}" -C "${d}/c" add -A
"${GITC[@]}" -C "${d}/c" commit -q -m local
run "${d}"
expect "S4 not fast-forward" "${OUT}" "fast-forward: NOT possible"

# S5: Cargo.lock と lefthook.yml の変更
d="$(setup s5)"; upstream_commit "${d}" Cargo.lock lock1; upstream_commit "${d}" lefthook.yml "x: 1"
run "${d}"
expect "S5 Cargo.lock category" "${OUT}" "Cargo.lock changes"
expect "S5 lefthook category" "${OUT}" "lefthook.yml changes"

# S6: 不正な BASE_REF
d="$(setup s6)"
run "${d}" BASE_REF=does-not-exist
expect_rc "S6 exit 2" 2

# S7: submodule（件名を出力しないこと・チェックアウトが古い場合は INFO）
d="$(setup s7)"
"${GITC[@]}" init -q "${d}/sub"
echo 1 >"${d}/sub/f"
"${GITC[@]}" -C "${d}/sub" add -A
"${GITC[@]}" -C "${d}/sub" commit -q -m "SECRET-SUBJECT-ONE"
"${GITC[@]}" -C "${d}/c" submodule add -q "${d}/sub" docs/spec 2>/dev/null
"${GITC[@]}" -C "${d}/c" commit -q -m "add sub"
echo 2 >"${d}/sub/f"
"${GITC[@]}" -C "${d}/sub" commit -q -am "SECRET-SUBJECT-TWO"
"${GITC[@]}" -C "${d}/c/docs/spec" fetch -q origin
"${GITC[@]}" -C "${d}/c/docs/spec" checkout -q --detach FETCH_HEAD
"${GITC[@]}" -C "${d}/c" add docs/spec
"${GITC[@]}" -C "${d}/c" commit -q -m "bump sub"
# チェックアウトを gitlink より古い commit へ戻す（実環境の docs/spec の状態を再現）
"${GITC[@]}" -C "${d}/c/docs/spec" checkout -q --detach HEAD~1
run "${d}"
expect "S7 submodule behind INFO" "${OUT}" "behind HEAD gitlink"
refute "S7 no subject leak (1)" "${OUT}" "SECRET-SUBJECT-ONE"
refute "S7 no subject leak (2)" "${OUT}" "SECRET-SUBJECT-TWO"

# S8: 取り込む側の変更が pipe バッファ（約 64KB）を超えても先頭付近の一致を見逃さない（SIGPIPE 偽陰性の回帰）
d="$(setup s8)"
mkdir -p "${d}/up/bulk"
for i in $(seq 1 20000); do : >"${d}/up/bulk/file_${i}.txt"; done
echo new >"${d}/up/a.txt"
echo lock1 >"${d}/up/Cargo.lock"
echo "x: 1" >"${d}/up/lefthook.yml"
echo added >"${d}/up/zz_added.txt"
"${GITC[@]}" -C "${d}/up" add -A
"${GITC[@]}" -C "${d}/up" commit -q -m "bulk"
"${GITC[@]}" -C "${d}/up" push -q origin HEAD:main
"${GITC[@]}" -C "${d}/c" fetch -q origin
echo local >"${d}/c/a.txt"
echo mine >"${d}/c/zz_added.txt"
run "${d}" STRICT=1
expect "S8 same-file HIGH (large list)" "${OUT}" "locally modified file also changed upstream: a.txt"
expect "S8 untracked collision HIGH (large list)" "${OUT}" "untracked file collides with a path added upstream (pull would abort): zz_added.txt"
expect "S8 Cargo.lock category (large list)" "${OUT}" "Cargo.lock changes"
expect "S8 lefthook category (large list)" "${OUT}" "lefthook.yml changes"
expect_rc "S8 STRICT exit 1" 1

# S9: staged rename の新パスが取り込む側の変更と一致する場合も検出する
d="$(setup s9)"; upstream_commit "${d}" b.txt b
"${GITC[@]}" -C "${d}/c" mv a.txt b.txt
run "${d}"
expect "S9 rename new path HIGH" "${OUT}" "locally modified file also changed upstream: b.txt"

if [ "${FAILS}" -ne 0 ]; then
  echo "${FAILS} check(s) failed"
  exit 1
fi
echo "all checks passed"
