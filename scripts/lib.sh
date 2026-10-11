#!/usr/bin/env bash
# scripts/*.sh（Makefile から呼ぶ薄い入口の実体）が共有するヘルパー。
# 直接実行せず source して使う:
#   DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
#   source "${DIR}/lib.sh"
#
# 呼び出し元は `set -euo pipefail` を引き継ぐ。失敗した終了コードはそのまま
# 呼び出し元（人・make・CI・lefthook）へ伝播する。プログラム出力は英語。
# 既存の bench_*.sh / check_*.sh は自前の骨格を持ち、本ファイルに依存しない。
set -euo pipefail

# リポジトリルート（本ファイルの親ディレクトリ）。cwd に依存せず解決する
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC2034 # source した側で参照する
readonly ROOT_DIR

# lint ツールの固定バージョン。CI（Fandhe-AI/actions の lint-docs reusable workflow）の
# 既定値に合わせる（CI 側が正。乖離したらこちらを追従させる）。
# EC_NPM_VERSION のみ npm ラッパーパッケージの版（CI は Go バイナリ release タグ v3.8.0 を
# 直接取得するため版番号体系が異なる。ローカル再現用の近似として npm 最新安定を固定する）。
# shellcheck disable=SC2034 # lint_docs.sh が参照する
readonly MARKDOWNLINT_VERSION="0.49.1" YAMLLINT_VERSION="1.38.0" EC_NPM_VERSION="6.1.1"
# shellcheck disable=SC2034
readonly COMMITLINT_VERSION="21.2.1" COMMITLINT_CONFIG_VERSION="21.2.0"
# 導入系ツールの固定バージョン（`=x.y.z` 完全固定方針に合わせ exact 固定）。
# CARGO_DENY_VERSION は Dockerfile の先行導入と値を同期させる
# shellcheck disable=SC2034
readonly LEFTHOOK_VERSION="2.1.10" CARGO_DENY_VERSION="0.20.2"

# die <message>: stderr にメッセージを出して終了コード 1 で終了する
die() {
    echo "ERROR: $*" >&2
    exit 1
}

# have_cmd <name>: PATH 上にコマンドがあれば 0
have_cmd() {
    command -v "$1" >/dev/null 2>&1
}
