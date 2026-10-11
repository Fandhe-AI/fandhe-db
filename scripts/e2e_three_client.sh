#!/usr/bin/env bash
# psql/psycopg/pg 実クライアント統合テスト（`make e2e-three-client` の実体。opt-in・ci 対象外）。
# 要 psql・python3+psycopg・node+pg。PSQL_BIN/PYTHON_BIN/NODE_BIN で上書き可。
# Issue #705: `three_client_e2e` は `fault-injection` feature 付きでビルドし、
# #702 の commit 後 panic ケースが `--fault-inject` を使えるようにする。
set -euo pipefail

cargo test -p fandhe-db-wire-server --features fault-injection --test three_client_e2e -- --ignored
cargo test -p fandhe-db-wire-server --test extended_syntax_e2e -- --ignored
cargo test -p fandhe-db-wire-server --test three_client_extended_e2e -- --ignored
