#!/usr/bin/env bash
# Issue #1303: テストが一時領域（TMPDIR 配下）へ作るディレクトリ・ファイルを終了時に
# 残さないことを検証する回帰ゲート。
#
# 専用の空ディレクトリ（mktemp -d）を TMPDIR に指定して代表テストを実行し、終了後に
# そのディレクトリが空であることを確認する。残置があれば一覧を出して失敗する。
# 削除対象は本スクリプトが作成したディレクトリのみで、システムの /tmp を走査・
# 一括削除することはない（他プロジェクトの一時領域に触れない）。
# Makefile の tmp-leak-check ターゲット・CI の tmp-leak-check ジョブから呼ばれる。
#
# 使い方: scripts/check_tmp_leak.sh        # 代表テストのサブセット
#         FULL=1 scripts/check_tmp_leak.sh # cargo test --workspace --all-features 全体

set -euo pipefail

# ビルドの一時ファイル（rustc / linker）を計測に混ぜないため、先に通常の TMPDIR でビルドする。
if [ "${FULL:-0}" = "1" ]; then
  cargo test --workspace --all-features --no-run
else
  cargo test -p fandhe-vector-db-wire-server --no-run
  cargo test -p fandhe-vector-db-engine --lib --no-run
fi

LEAK_DIR="$(mktemp -d)"
trap 'rm -rf "${LEAK_DIR}"' EXIT

if [ "${FULL:-0}" = "1" ]; then
  TMPDIR="${LEAK_DIR}" cargo test --workspace --all-features
else
  # in-process + user store / throwaway DB / 子プロセス起動 / TLS フィクスチャ /
  # ガード自身の回帰テスト / wire-server の auth unit test / engine の temp_db。
  TMPDIR="${LEAK_DIR}" cargo test -p fandhe-vector-db-wire-server \
    --test wire1_simple_query \
    --test wire_auth \
    --test wire_framing \
    --test wire_limits \
    --test http6_auth_failure \
    --test wire_fault_injection_cli \
    --test http10_tls_surface \
    --test tmp_fixture_cleanup
  TMPDIR="${LEAK_DIR}" cargo test -p fandhe-vector-db-wire-server --lib auth::
  TMPDIR="${LEAK_DIR}" cargo test -p fandhe-vector-db-engine --lib test_util
fi

# 外部ツール由来で除外が必要なエントリが出た場合のみ、理由コメント付きでここへ列挙する。
LEFTOVER="$(find "${LEAK_DIR}" -mindepth 1 -maxdepth 1 | sort)"
if [ -n "${LEFTOVER}" ]; then
  COUNT="$(printf '%s\n' "${LEFTOVER}" | wc -l)"
  echo "ERROR: tests left ${COUNT} temp entries under TMPDIR:" >&2
  printf '%s\n' "${LEFTOVER}" | head -50 >&2
  exit 1
fi
echo "OK: no leftover temp entries"
