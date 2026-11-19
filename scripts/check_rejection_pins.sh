#!/usr/bin/env bash
# 拒否固定テストの注記検査（Issue #3164）の入口。
#
# 役割: ADR docs/design/rejection-pin-annotation.md の D2〜D9 を `git diff <base>...HEAD` に
#       適用し、注記（tracked: / intentional:）の無い拒否固定テストと書式不正の注記を検出する。
#       判定の本体は同じディレクトリの check_rejection_pins.py（標準ライブラリのみ）。
#       本スクリプトは使用法・基準 ref の検証・Python 中核の起動だけを受け持つ。
# 呼び出し元: 人。Makefile・CI への結線は後続の #3165 で行う。
# 使い方: scripts/check_rejection_pins.sh <base-ref>   例: scripts/check_rejection_pins.sh origin/main
# 終了コード: 0 = 違反なし / 1 = 違反あり・検査不能（fail-closed） / 2 = 使用法の誤り
# 前提: オフラインで動く（ネットワーク・gh は使わない）。HEAD のコミット内容だけを読み、
#       作業ツリーの未コミット変更は見ない。shallow clone では merge-base が取れず失敗する。
#       検査を弱める環境変数・上書き経路は設けない。
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

usage() {
    echo "usage: check_rejection_pins.sh <base-ref>" >&2
    exit 2
}

if [[ $# -ne 1 || -z "$1" || "$1" == -* ]]; then
    usage
fi
base="$1"

have_cmd python3 || die "python3 not found (required by check_rejection_pins.py)"

cd "${ROOT_DIR}"

# 基準 ref を SHA へ解決する（ref 名をそのまま下流へ渡さない）
if ! base_sha="$(git rev-parse --verify --quiet --end-of-options "${base}^{commit}")"; then
    echo "unknown base ref: ${base}" >&2
    exit 2
fi

exec python3 "${DIR}/check_rejection_pins.py" --repo "${ROOT_DIR}" --base "${base_sha}"
