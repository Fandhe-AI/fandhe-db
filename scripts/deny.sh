#!/usr/bin/env bash
# 依存監査（`make deny` の実体）。cargo-deny 未導入なら固定版を自動導入する。
# CARGO_DENY_VERSION は Dockerfile の先行導入と値を同期させる（lib.sh 参照）。
# deny.toml が無い場合はスキップする（CI の rust-ci / cargo deny check と同一のチェック集合）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

cd "${ROOT_DIR}"
if [ ! -f Cargo.toml ] || [ ! -f deny.toml ]; then
    echo "skip: Cargo.toml or deny.toml missing; skipping deny"
    exit 0
fi

export PATH="${HOME}/.cargo/bin:${PATH}"
if ! have_cmd cargo-deny; then
    echo "installing cargo-deny ${CARGO_DENY_VERSION}"
    cargo install "cargo-deny@${CARGO_DENY_VERSION}" --locked
fi
cargo deny --locked check advisories bans licenses sources
