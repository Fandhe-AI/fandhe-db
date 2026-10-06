#!/usr/bin/env bash
# Issue #1329: メイン worktree が BASE_REF（既定 origin/main）からどれだけ遅れているかと、
# pull 前のリスク（作業消失・競合・環境更新の要否）を読み取り専用で評価する診断スクリプト。
#
# 読み取り専用の保証: fetch / pull / checkout / stash / clean / submodule update / reset を
# 一切実行しない。ネットワークは REMOTE_CHECK=1 のときだけ `git ls-remote`（ローカル ref を
# 更新しない）を使う。一時ファイルは作らない。
# private submodule（docs/spec）の漏えい防止のため、submodule のコミット件名は出さず
# SHA と件数だけを出す（spec-confidentiality）。出力パスは repo 相対のみ。
# Makefile の worktree-lag-check ターゲットから呼ばれる。判断基準は
# docs/design/main-worktree-lag-assessment.md を参照。
#
# 環境変数: REPO（既定: カレントの toplevel）/ BASE_REF（既定: origin/main）/
#           REMOTE_CHECK=1 / STRICT=1（HIGH があれば exit 1）
# 終了コード: 0=レポート出力済み、1=STRICT で HIGH 検出、2=入力不正

set -euo pipefail

BASE_REF="${BASE_REF:-origin/main}"
REPO="${REPO:-$(git rev-parse --show-toplevel)}"

if [ ! -d "${REPO}" ] || ! git -C "${REPO}" rev-parse --git-dir >/dev/null 2>&1; then
  echo "error: REPO is not a git worktree" >&2
  exit 2
fi
g() { git -C "${REPO}" -c core.quotepath=off "$@"; }
REPO_TOP="$(g rev-parse --show-toplevel)"

if ! BASE_SHA="$(g rev-parse --verify --quiet "${BASE_REF}^{commit}")"; then
  echo "error: BASE_REF does not resolve to a commit" >&2
  exit 2
fi
HEAD_SHA="$(g rev-parse --verify "HEAD^{commit}")"

HIGH=0
MEDIUM=0
INFO=0
high() { HIGH=$((HIGH + 1)); echo "  [HIGH] $*"; }
medium() { MEDIUM=$((MEDIUM + 1)); echo "  [MEDIUM] $*"; }
info() { INFO=$((INFO + 1)); echo "  [INFO] $*"; }

# 取り込む側の変更（HEAD と BASE の 2 点差分）
INCOMING="$(g diff --name-only "${HEAD_SHA}" "${BASE_SHA}" --)"
INCOMING_ADDED="$(g diff --name-only --diff-filter=A "${HEAD_SHA}" "${BASE_SHA}" --)"
# 注意: pipefail 下では `printf | grep -q` が grep の早期終了で SIGPIPE となり偽陰性になる
# （入力が pipe バッファを超える大量変更時）。here-string で入力を渡して回避する。
has_incoming() { grep -Eq "$1" <<<"${INCOMING}"; }
# submodule（gitlink）のパス一覧。状態は Submodules 節で評価するため Tracked changes からは除外する
SUBS="$(g ls-tree -r HEAD | awk '$1 == "160000" { print $4 }')"

echo "== Summary =="
read -r BEHIND AHEAD < <(g rev-list --left-right --count "${BASE_SHA}...${HEAD_SHA}")
echo "  HEAD: ${HEAD_SHA}"
echo "  BASE: ${BASE_SHA} (${BASE_REF})"
echo "  behind: ${BEHIND}, ahead: ${AHEAD}"
if [ "${AHEAD}" -eq 0 ]; then
  echo "  fast-forward: possible"
else
  echo "  fast-forward: NOT possible"
  high "local branch is ahead of ${BASE_REF} by ${AHEAD} commit(s); pull --ff-only would fail"
fi
if [ "${REMOTE_CHECK:-0}" = "1" ]; then
  REMOTE_SHA="$(g ls-remote origin refs/heads/main 2>/dev/null | cut -f1 | head -n1 || true)"
  if [ -z "${REMOTE_SHA}" ]; then
    echo "  remote main: UNKNOWN (ls-remote failed)"
  elif [ "${REMOTE_SHA}" = "${BASE_SHA}" ]; then
    echo "  remote main: ${REMOTE_SHA} (local ${BASE_REF} is up to date)"
  else
    echo "  remote main: ${REMOTE_SHA} (local ${BASE_REF} is STALE; run fetch before the final decision)"
    info "local ${BASE_REF} is stale; real lag is larger than reported"
  fi
fi

echo "== Tracked changes =="
# -z 出力: rename は "XY new\0old\0" の 2 トークンになり、特殊文字パスもクォートされない
HAS_TRACKED=0
while IFS= read -r -d '' entry; do
  xy="${entry:0:2}"
  path="${entry:3}"
  case "${xy}" in
    R* | C* | *R | *C) IFS= read -r -d '' _orig || true ;;
  esac
  if grep -Fxq -- "${path}" <<<"${SUBS}"; then continue; fi
  HAS_TRACKED=1
  if grep -Fxq -- "${path}" <<<"${INCOMING}"; then
    high "locally modified file also changed upstream: ${path}"
  else
    info "locally modified file (not touched upstream): ${path}"
  fi
done < <(g status --porcelain=v1 -z --untracked-files=no)
if [ "${HAS_TRACKED}" -eq 0 ]; then
  echo "  none (excluding submodule gitlink state)"
fi

echo "== Untracked collisions =="
UNTRACKED="$(g ls-files --others --exclude-standard)"
if [ -z "${UNTRACKED}" ]; then
  echo "  none"
else
  while IFS= read -r path; do
    if grep -Fxq -- "${path}" <<<"${INCOMING_ADDED}"; then
      high "untracked file collides with a path added upstream (pull would abort): ${path}"
    else
      info "untracked file (no collision): ${path}"
    fi
  done <<<"${UNTRACKED}"
fi

echo "== Submodules =="
if [ -z "${SUBS}" ]; then
  echo "  none"
fi
while IFS= read -r sub; do
  [ -n "${sub}" ] || continue
  head_link="$(g ls-tree HEAD -- "${sub}" | awk '{print $3}')"
  base_link="$(g ls-tree "${BASE_SHA}" -- "${sub}" | awk '{print $3}')"
  echo "  ${sub}: HEAD gitlink=${head_link:-none} BASE gitlink=${base_link:-none}"
  if [ -n "${head_link}" ] && [ -n "${base_link}" ] && [ "${head_link}" != "${base_link}" ]; then
    medium "submodule ${sub} gitlink changes upstream; run submodule update after pull"
  fi
  # 未初期化の submodule は空ディレクトリで、git が親リポへ解決してしまうため toplevel 一致で判定する
  sub_top="$(git -C "${REPO}/${sub}" rev-parse --show-toplevel 2>/dev/null || true)"
  if [ "${sub_top}" != "${REPO_TOP}/${sub}" ]; then
    info "submodule ${sub} is not checked out"
    continue
  fi
  cur="$(git -C "${REPO}/${sub}" rev-parse HEAD 2>/dev/null || true)"
  echo "    checked out: ${cur:-unknown}"
  if [ -z "${cur}" ] || [ -z "${head_link}" ]; then
    info "submodule ${sub} state UNKNOWN"
    continue
  fi
  if [ -n "$(git -C "${REPO}/${sub}" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    high "submodule ${sub} has local uncommitted changes"
  fi
  if [ "${cur}" = "${head_link}" ]; then
    echo "    relation: in sync with HEAD gitlink"
  elif ! git -C "${REPO}/${sub}" cat-file -e "${head_link}^{commit}" 2>/dev/null; then
    info "submodule ${sub}: gitlink object not fetched (relation UNKNOWN)"
  elif git -C "${REPO}/${sub}" merge-base --is-ancestor "${cur}" "${head_link}" 2>/dev/null; then
    n="$(git -C "${REPO}/${sub}" rev-list --count "${cur}..${head_link}")"
    info "submodule ${sub}: checkout is ${n} commit(s) behind HEAD gitlink (not a local edit; submodule update needed)"
  elif git -C "${REPO}/${sub}" merge-base --is-ancestor "${head_link}" "${cur}" 2>/dev/null; then
    n="$(git -C "${REPO}/${sub}" rev-list --count "${head_link}..${cur}")"
    high "submodule ${sub}: checkout is ${n} local commit(s) ahead of HEAD gitlink"
  else
    high "submodule ${sub}: checkout diverged from HEAD gitlink"
  fi
done <<<"${SUBS}"

echo "== Incoming change categories =="
echo "  incoming changed files: $(printf '%s\n' "${INCOMING}" | grep -c . || true)"
if has_incoming '^Cargo\.lock$'; then medium "Cargo.lock changes (dependency update; rebuild required)"; fi
if has_incoming '(^|/)Cargo\.toml$'; then
  dep_lines="$(g diff -U0 "${HEAD_SHA}" "${BASE_SHA}" -- ':(glob)**/Cargo.toml' | grep -Ec '^[+-][a-zA-Z0-9_-]+ *= *["{]' || true)"
  if [ "${dep_lines}" -gt 0 ]; then
    medium "Cargo.toml has ${dep_lines} dependency-like line change(s); review before building"
  else
    info "Cargo.toml changes (no dependency-like lines detected)"
  fi
fi
if has_incoming '^rust-toolchain\.toml$'; then medium "rust-toolchain.toml changes (rustup will fetch a new toolchain)"; fi
if has_incoming '^lefthook\.yml$'; then medium "lefthook.yml changes (re-run make hooks)"; fi
if has_incoming '^(Makefile|commitlint\.config\.mjs|\.github/workflows/)'; then info "Makefile / CI / commitlint config changes"; fi
if has_incoming '^(skills-lock\.json|\.agents/skills/|\.claude/)'; then info "skills / .claude changes"; fi
if has_incoming '^deny\.toml$'; then info "deny.toml changes"; fi

echo "== Stash / worktrees =="
echo "  stash entries: $(g stash list | grep -c . || true) (shared by all worktrees; pull does not touch them)"
echo "  linked worktrees: $(g worktree list --porcelain | grep -c '^worktree ' || true) (including the main one)"

echo "== Risk verdict =="
echo "  HIGH=${HIGH} MEDIUM=${MEDIUM} INFO=${INFO}"
if [ "${HIGH}" -eq 0 ] && [ "${AHEAD}" -eq 0 ]; then
  echo "  recommendation: git pull --ff-only, then submodule update / make hooks as flagged above"
else
  echo "  recommendation: do NOT pull yet; resolve the HIGH items individually"
fi
if [ "${STRICT:-0}" = "1" ] && [ "${HIGH}" -gt 0 ]; then
  exit 1
fi
