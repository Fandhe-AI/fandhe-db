#!/usr/bin/env bash
# 親 Issue の close 前に、子 PR 本文の「対象外」節と受け皿 Issue を突合する薄い入口。
# 実体は scripts/check_issue_closeout.py（解析・分類・循環検出）。
#
# 使い方:
#   scripts/check_issue_closeout.sh <親 Issue 番号> [--repo OWNER/NAME] [--max-hops N]
#                                   [--max-nodes N] [--verbose]
#   scripts/check_issue_closeout.sh --check-closed   # close 済み Issue を指す tracked: 注記を列挙
#
# gh の認証とネットワークが要るため make・CI には載せない手動ツール（Issue #3168）。
# 終了コード: 0=検出なし / 1=検出あり / 2=使用法の誤り / 3=取得の失敗
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib.sh
source "${DIR}/lib.sh"

have_cmd gh || die "gh CLI is required (https://cli.github.com/)"
have_cmd python3 || die "python3 is required"

exec python3 "${ROOT_DIR}/scripts/check_issue_closeout.py" --root "${ROOT_DIR}" "$@"
