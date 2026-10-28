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

# 代表テストの対象一覧（事前ビルドと計測実行の両方がここを参照する単一定義）。
# in-process + user store / throwaway DB / 子プロセス起動 / TLS フィクスチャ /
# ガード自身の回帰テスト / 一時表（Issue #1685・TABLE-44）の wire 結合テスト。
# wire_temp_table は一時ストアがディスク（TMPDIR）へ spill しないことを見る。
WIRE_TESTS=(
  wire1_simple_query
  wire_auth
  wire_framing
  wire_limits
  http6_auth_failure
  wire_fault_injection_cli
  http10_tls_surface
  tmp_fixture_cleanup
  wire_temp_table
)
# 一時表（Issue #1685・TABLE-44）の engine 結合テスト。
ENGINE_TESTS=(
  table44_temp_table_ddl
  table44_temp_table_dml
  table44_temp_table_isolation
)
# lib のユニットテストは名前フィルタで絞る（wire-server の auth unit test・engine の temp_db）。
WIRE_LIB_FILTER="auth::"
ENGINE_LIB_FILTER="test_util"

# 上記一覧を `--test <name>` の引数列へ展開する。
WIRE_TEST_ARGS=()
for name in "${WIRE_TESTS[@]}"; do
  WIRE_TEST_ARGS+=(--test "${name}")
done
ENGINE_TEST_ARGS=()
for name in "${ENGINE_TESTS[@]}"; do
  ENGINE_TEST_ARGS+=(--test "${name}")
done

# ビルドの一時ファイル（rustc / linker）を計測に混ぜないため、先に通常の TMPDIR でビルドする。
# 既定モードでは計測で実行するテストバイナリ（上記一覧と lib のユニットテスト）だけを
# ビルドし、実行しない数百本の結合テストまでビルドしない。下の計測実行と同じ
# パッケージ・feature 指定のため、計測時に再ビルド（＝ TMPDIR への一時ファイル）は起きない。
if [ "${FULL:-0}" = "1" ]; then
  cargo test --workspace --all-features --no-run
else
  cargo test -p fandhe-db-wire-server --lib "${WIRE_TEST_ARGS[@]}" --no-run
  cargo test -p fandhe-db-engine --lib "${ENGINE_TEST_ARGS[@]}" --no-run
fi

LEAK_DIR="$(mktemp -d)"
trap 'rm -rf "${LEAK_DIR}"' EXIT

if [ "${FULL:-0}" = "1" ]; then
  TMPDIR="${LEAK_DIR}" cargo test --workspace --all-features
else
  TMPDIR="${LEAK_DIR}" cargo test -p fandhe-db-wire-server "${WIRE_TEST_ARGS[@]}"
  TMPDIR="${LEAK_DIR}" cargo test -p fandhe-db-wire-server --lib "${WIRE_LIB_FILTER}"
  TMPDIR="${LEAK_DIR}" cargo test -p fandhe-db-engine --lib "${ENGINE_LIB_FILTER}"
  TMPDIR="${LEAK_DIR}" cargo test -p fandhe-db-engine "${ENGINE_TEST_ARGS[@]}"
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
