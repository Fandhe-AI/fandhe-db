#!/usr/bin/env bash
# lefthook の git hooks を導入する（`make hooks` の実体）。
# lefthook（Go 製。crates.io には存在しないため cargo フォールバックは置かない）は
# brew（バージョン固定不可だが常用導線）を優先し、無ければ npm 配布版を exact 固定の
# npx ワンショットで実行する（lefthook が生成する hook スクリプトは PATH → npx の順で
# 本体を解決するため、npx 経由の導入でもコミット時にフックが機能する）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

if have_cmd lefthook; then
    lefthook install
elif have_cmd brew; then
    echo "installing lefthook via brew"
    brew install lefthook && lefthook install
elif have_cmd npx; then
    echo "installing hooks via npx lefthook@${LEFTHOOK_VERSION}"
    npx --yes "lefthook@${LEFTHOOK_VERSION}" install
else
    echo "brew / npx not found. See https://lefthook.dev/installation/" >&2
    exit 1
fi
