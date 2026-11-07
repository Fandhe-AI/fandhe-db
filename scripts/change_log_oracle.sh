#!/usr/bin/env bash
# Issue #3069（RECOVER-14 のポインタ。設計は docs/design/change-log-pitr.md の D1 の層 3・D11）の
# 変更ログのオラクル試験を、engine の結合テスト全体で強制有効化して流す。
#
# feature `change-log-oracle` と環境変数 FANDHE_CHANGE_LOG_ORACLE で、永続 `Storage` の open 時に変更ログを
# 強制的に有効にする。2 段で流す:
#   1 段目: FANDHE_CHANGE_LOG_ORACLE=1（強制有効化 + 各 commit のレコードと実差分の突き合わせ）。
#           次のテストは有効化そのものが差になる、または突き合わせが O(DB サイズ) / commit のため
#           除外する（期待値は変えない。除外はテスト名で行う）:
#             - persist3_on_disk_row_entry_layout_via_raw_redb: redb の表一覧を「rows と世代表だけ」と固定している
#             - *_latency_is_independent_of_row_count（4 件）: 行数が増えても遅延が増えないことを測るため、
#               O(DB) の突き合わせでは落ちる（2 段目で突き合わせなしで流す）
#   2 段目: FANDHE_CHANGE_LOG_ORACLE=force（強制有効化のみ）で上記の遅延 4 件だけを流す。
#           有効時の実コストが DB サイズに依存しないことの確認になる。
# Makefile の change-log-oracle ターゲット・CI の change-log-oracle ジョブから呼ばれる。
#
# 使い方: scripts/change_log_oracle.sh

set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${DIR}/lib.sh"
cd "${ROOT_DIR}"

if [ "$#" -ne 0 ]; then
    echo "usage: scripts/change_log_oracle.sh (no arguments)" >&2
    exit 2
fi

echo "== stage 1: forced change log + oracle (FANDHE_CHANGE_LOG_ORACLE=1)"
FANDHE_CHANGE_LOG_ORACLE=1 cargo test -p fandhe-db-engine --all-features --no-fail-fast --test '*' -- \
    --skip persist3_on_disk_row_entry_layout_via_raw_redb \
    --skip latency_is_independent_of_row_count

echo "== stage 2: forced change log without the oracle (FANDHE_CHANGE_LOG_ORACLE=force), latency tests"
FANDHE_CHANGE_LOG_ORACLE=force cargo test -p fandhe-db-engine --all-features --no-fail-fast \
    --test catalog --test catalog_latency --test index7_create_index_latency -- \
    latency_is_independent_of_row_count
