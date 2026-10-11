#!/usr/bin/env bash
# ドキュメント／設定ファイル系 lint を 1 種ずつ実行する（`make lint-md|lint-yaml|
# lint-editorconfig|lint-commits` の実体）。CI の lint-docs ジョブと同等の内容。
# 使い方: scripts/lint_docs.sh <md|yaml|editorconfig|commits>
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

cd "${ROOT_DIR}"

case "${1:-}" in
    md)
        # .markdownlint.jsonc / .markdownlintignore 参照
        npx --yes "markdownlint-cli@${MARKDOWNLINT_VERSION}" --ignore-path .markdownlintignore "**/*.md"
        ;;
    yaml)
        # yamllint は Python 製のため npx で賄えない。導入済みの実体（brew / pip）を優先し、
        # uvx があれば固定版のワンショット実行で代替する。いずれも無ければ fail-closed で
        # 導入方法を案内して失敗する（silent skip は CI との false-green 乖離になるため行わない）。
        if have_cmd yamllint; then
            yamllint .
        elif have_cmd uvx; then
            uvx "yamllint==${YAMLLINT_VERSION}" .
        else
            echo "yamllint not installed: brew install yamllint / pip install yamllint==${YAMLLINT_VERSION}" >&2
            exit 1
        fi
        ;;
    editorconfig)
        # .editorconfig + .editorconfig-checker.json 参照
        npx --yes "editorconfig-checker@${EC_NPM_VERSION}"
        ;;
    commits)
        # main からの分岐点以降のコミットを CI（lint-docs の commitlint ジョブ）と同じ
        # extends 構成で検証する。origin/main が未取得の環境では範囲を決められないためスキップする。
        if git rev-parse --verify origin/main >/dev/null 2>&1; then
            npx --yes -p "@commitlint/cli@${COMMITLINT_VERSION}" -p "@commitlint/config-conventional@${COMMITLINT_CONFIG_VERSION}" \
                commitlint --extends @commitlint/config-conventional --from "$(git merge-base origin/main HEAD)" --to HEAD
        else
            echo "skip: origin/main not fetched; skipping commitlint"
        fi
        ;;
    *)
        echo "usage: lint_docs.sh <md|yaml|editorconfig|commits>" >&2
        exit 2
        ;;
esac
