#!/usr/bin/env bash
# precision 判断材料レポートとパラメータ感度スイープ（`make precision-report` の実体。TASK-163）。
# 実測値を標準出力へ出すためローカル専用。GITHUB_ACTIONS 下ではテスト側が fail-closed で拒否する。
set -euo pipefail

cargo test --release -p fandhe-db-engine --test precision_eval -- --ignored --nocapture --exact precision_eval_report
cargo test --release -p fandhe-db-engine --test precision_eval -- --ignored --nocapture --exact precision_eval_policy_sweep
