#!/usr/bin/env bash
# rustup（cargo）を未導入の場合のみ導入する（`make rustup` の実体）。再実行しても安全。
set -euo pipefail

if ! command -v rustup >/dev/null 2>&1 && [ ! -x "${HOME}/.cargo/bin/rustup" ]; then
    echo "installing rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
