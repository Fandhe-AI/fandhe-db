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
#           REMOTE_CHECK=1（鮮度確認。未指定だと判定は DEFERRED。BASE_REF は <remote>/<branch> 形式必須）/ STRICT=1（HIGH があれば exit 1）
# 終了コード: 0=レポート出力済み、1=STRICT で HIGH 検出、2=入力不正

set -euo pipefail

# `git status` 等が index を opportunistic に更新（書き込み）しないよう、サブモジュール内を含む
# 全 git 呼び出しで optional lock を無効化する（読み取り専用の保証。環境変数は子 git へ継承される）。
export GIT_OPTIONAL_LOCKS=0

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

# 取り込む側の変更（HEAD と BASE の 2 点差分）。パスは NUL 区切りで配列へ読み込み、改行・引用符を含む
# パスでも 1 パス 1 要素のまま比較する。--no-renames で rename を「旧パス削除 + 新パス追加」に分解し、
# 旧パス側の変更（INCOMING）と新パス側の追加（INCOMING_ADDED）の両方を取りこぼさない。
INCOMING=()
while IFS= read -r -d '' _p; do INCOMING+=("${_p}"); done \
  < <(g diff --name-only --no-renames -z "${HEAD_SHA}" "${BASE_SHA}" --)
INCOMING_ADDED=()
while IFS= read -r -d '' _p; do INCOMING_ADDED+=("${_p}"); done \
  < <(g diff --name-only --no-renames --diff-filter=A -z "${HEAD_SHA}" "${BASE_SHA}" --)
# カテゴリ判定（正規表現）専用の改行区切り文字列。パス単位の照合には使わない。
# 注意: pipefail 下では `printf | grep -q` が grep の早期終了で SIGPIPE となり偽陰性になる
# （入力が pipe バッファを超える大量変更時）。here-string で入力を渡して回避する。
INCOMING_TEXT="$(printf '%s\n' ${INCOMING[@]+"${INCOMING[@]}"})"
has_incoming() { grep -Eq "$1" <<<"${INCOMING_TEXT}"; }
# in_list <needle> <要素...>: 完全一致で含まれるか（空配列は ${a[@]+"${a[@]}"} 形式で渡す）
in_list() {
  local needle="$1" x
  shift
  for x in "$@"; do
    if [ "${x}" = "${needle}" ]; then return 0; fi
  done
  return 1
}
# collides <untracked パス> <上流の追加パス>: 完全一致、または一方が他方の親ディレクトリである場合に真。
# untracked の `foo/bar` と上流追加の `foo`（ファイル）、untracked の `foo`（ファイル / 入れ子 repo）と
# 上流追加の `foo/bar` はいずれも pull（merge）が中断される。
collides() {
  local u="${1%/}" a="${2%/}"
  if [ "${u}" = "${a}" ]; then return 0; fi
  case "${u}" in "${a}"/*) return 0 ;; esac
  case "${a}" in "${u}"/*) return 0 ;; esac
  return 1
}
# submodule（gitlink）のパス一覧（NUL 区切りで配列化）。状態は Submodules 節で評価するため
# Tracked changes からは除外する
SUBS=()
while IFS= read -r -d '' _rec; do
  _meta="${_rec%%$'\t'*}"
  case "${_meta}" in 160000\ *) SUBS+=("${_rec#*$'\t'}") ;; esac
done < <(g ls-tree -r -z HEAD)
# BASE 側で新規追加される submodule も評価対象に含める（HEAD の gitlink だけだと追随作業を見落とす）
while IFS= read -r -d '' _rec; do
  _meta="${_rec%%$'\t'*}"
  case "${_meta}" in
    160000\ *)
      _sp="${_rec#*$'\t'}"
      if ! in_list "${_sp}" ${SUBS[@]+"${SUBS[@]}"}; then SUBS+=("${_sp}"); fi
      ;;
  esac
done < <(g ls-tree -r -z "${BASE_SHA}")
# 取り込み元の鮮度が確認できない（REMOTE_CHECK で STALE / 確認失敗）場合 1 にし、最終判定を保留する
VERDICT_DEFERRED=0

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
# BASE_REF が `<remote>/<branch>` 形式のリモート追跡 ref のとき、その remote / branch を導出する
# （REMOTE_CHECK の照会先と、推奨する pull コマンドの取り込み先の両方に使う。最長一致の remote 名を採用）。
REMOTE_NAME=""
REMOTE_BRANCH=""
while IFS= read -r _r; do
  case "${BASE_REF}" in
    "${_r}"/?*)
      if [ "${#_r}" -gt "${#REMOTE_NAME}" ]; then
        REMOTE_NAME="${_r}"
        REMOTE_BRANCH="${BASE_REF#"${_r}"/}"
      fi
      ;;
  esac
done < <(g remote)
if [ -n "${REMOTE_NAME}" ] && ! g rev-parse --verify --quiet "refs/remotes/${BASE_REF}" >/dev/null; then
  REMOTE_NAME=""
  REMOTE_BRANCH=""
fi
if [ "${REMOTE_CHECK:-0}" != "1" ]; then
  # 鮮度未確認のまま pull を推奨しない（ローカルの追跡 ref が古いと実際の遅れ・競合を過小評価する）
  echo "  remote freshness: NOT CHECKED (set REMOTE_CHECK=1 or run git fetch)"
  VERDICT_DEFERRED=1
else
  if [ -z "${REMOTE_NAME}" ]; then
    echo "error: REMOTE_CHECK=1 requires BASE_REF to be a remote-tracking ref (<remote>/<branch>)" >&2
    exit 2
  fi
  REMOTE_SHA="$(g ls-remote "${REMOTE_NAME}" "refs/heads/${REMOTE_BRANCH}" 2>/dev/null | cut -f1 | head -n1 || true)"
  if [ -z "${REMOTE_SHA}" ]; then
    echo "  remote ${REMOTE_BRANCH}: UNKNOWN (ls-remote failed)"
    VERDICT_DEFERRED=1
  elif [ "${REMOTE_SHA}" = "${BASE_SHA}" ]; then
    echo "  remote ${REMOTE_BRANCH}: ${REMOTE_SHA} (local ${BASE_REF} is up to date)"
  else
    echo "  remote ${REMOTE_BRANCH}: ${REMOTE_SHA} (local ${BASE_REF} is STALE; run fetch before the final decision)"
    info "local ${BASE_REF} is stale; real lag is larger than reported"
    VERDICT_DEFERRED=1
  fi
fi

echo "== Tracked changes =="
# -z 出力: rename / copy は "XY new\0old\0" の 2 トークンになり、特殊文字パスもクォートされない。
# 旧パス（_orig）も上流の変更と照合する（上流が元パスを変更していれば pull で競合する）。
HAS_TRACKED=0
while IFS= read -r -d '' entry; do
  xy="${entry:0:2}"
  path="${entry:3}"
  orig=""
  case "${xy}" in
    R* | C* | *R | *C) IFS= read -r -d '' orig || true ;;
  esac
  if in_list "${path}" ${SUBS[@]+"${SUBS[@]}"}; then continue; fi
  HAS_TRACKED=1
  if in_list "${path}" ${INCOMING[@]+"${INCOMING[@]}"}; then
    high "locally modified file also changed upstream: ${path}"
  elif [ -n "${orig}" ] && in_list "${orig}" ${INCOMING[@]+"${INCOMING[@]}"}; then
    high "locally renamed file's original path also changed upstream: ${orig} (renamed to ${path})"
  else
    info "locally modified file (not touched upstream): ${path}"
  fi
done < <(g status --porcelain=v1 -z --untracked-files=no)
if [ "${HAS_TRACKED}" -eq 0 ]; then
  echo "  none (excluding submodule gitlink state)"
fi

echo "== Untracked collisions =="
UNTRACKED=()
while IFS= read -r -d '' _p; do UNTRACKED+=("${_p}"); done < <(g ls-files --others --exclude-standard -z)
if [ "${#UNTRACKED[@]}" -eq 0 ]; then
  echo "  none"
else
  for path in "${UNTRACKED[@]}"; do
    hit=""
    for added in ${INCOMING_ADDED[@]+"${INCOMING_ADDED[@]}"}; do
      if collides "${path}" "${added}"; then hit="${added}"; break; fi
    done
    if [ -n "${hit}" ]; then
      high "untracked path collides with a path added upstream (pull would abort): ${path} (upstream: ${hit})"
    else
      info "untracked path (no collision): ${path}"
    fi
  done
fi
# ignored ファイルは --exclude-standard の untracked 一覧に出ないが、上流が同一パスを追跡対象として
# 追加すると merge で上書きされ得る（ローカルの ignored データが消える）。ディレクトリ単位
# （--directory）で列挙し、上流の追加パスとの衝突を HIGH として扱う。
IGNORED=()
while IFS= read -r -d '' _p; do IGNORED+=("${_p}"); done \
  < <(g ls-files --others --ignored --exclude-standard --directory -z)
for path in ${IGNORED[@]+"${IGNORED[@]}"}; do
  for added in ${INCOMING_ADDED[@]+"${INCOMING_ADDED[@]}"}; do
    if collides "${path}" "${added}"; then
      high "ignored local path collides with a path added upstream (pull would overwrite it): ${path} (upstream: ${added})"
      break
    fi
  done
done

echo "== Submodules =="
if [ "${#SUBS[@]}" -eq 0 ]; then
  echo "  none"
fi
for sub in ${SUBS[@]+"${SUBS[@]}"}; do
  head_link="$(g ls-tree HEAD -- "${sub}" | awk '{print $3}')"
  base_link="$(g ls-tree "${BASE_SHA}" -- "${sub}" | awk '{print $3}')"
  echo "  ${sub}: HEAD gitlink=${head_link:-none} BASE gitlink=${base_link:-none}"
  if [ -n "${head_link}" ] && [ -n "${base_link}" ] && [ "${head_link}" != "${base_link}" ]; then
    medium "submodule ${sub} gitlink changes upstream; run submodule update after pull"
  fi
  if [ -z "${head_link}" ] && [ -n "${base_link}" ]; then
    medium "submodule ${sub} is added upstream; run submodule update --init after pull"
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
  # untracked も評価対象にする（submodule update / checkout が未追跡ファイルで中断・汚染されうる）
  if [ -n "$(git -C "${REPO}/${sub}" status --porcelain 2>/dev/null)" ]; then
    high "submodule ${sub} has local uncommitted or untracked changes"
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
done

echo "== Incoming change categories =="
echo "  incoming changed files: ${#INCOMING[@]}"
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
if [ "${VERDICT_DEFERRED}" -eq 1 ]; then
  echo "  recommendation: DEFERRED; ${BASE_REF} freshness is not confirmed (not checked, stale or ls-remote failed); run git fetch, then re-run with REMOTE_CHECK=1"
elif [ "${HIGH}" -eq 0 ] && [ "${AHEAD}" -eq 0 ]; then
  # 比較した BASE_REF と取り込み先を一致させるため、upstream 任せの引数なし pull ではなく取り込み先を明示する
  if [ -n "${REMOTE_NAME}" ]; then
    echo "  recommendation: git pull --ff-only ${REMOTE_NAME} ${REMOTE_BRANCH}, then submodule update / make hooks as flagged above"
  else
    echo "  recommendation: git merge --ff-only ${BASE_REF}, then submodule update / make hooks as flagged above"
  fi
else
  echo "  recommendation: do NOT pull yet; resolve the HIGH items individually"
fi
if [ "${STRICT:-0}" = "1" ] && [ "${HIGH}" -gt 0 ]; then
  exit 1
fi
