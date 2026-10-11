#!/usr/bin/env bash
# 開発環境を一括構築する（`make setup` の実体）。
# サブモジュール → rustup → lefthook の順を保証する。依存ターゲット並記だと
# `make -j` 実行時に順序が保証されず、cargo フォールバックを持つ hooks が rustup より
# 先に走りうるため、1 本のスクリプトで直列に呼ぶ（Fandhe-AI/rust-ai-library と同一方針）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

"${DIR}/init_submodule.sh"
"${DIR}/install_rustup.sh"
"${DIR}/install_hooks.sh"
echo "setup complete"
