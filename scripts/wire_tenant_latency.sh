#!/usr/bin/env bash
# 他テナント保持 id と未存在 id への INSERT の wire 往復レイテンシ分布の同一性検証
# （`make wire-tenant-latency` の実体。Issue #738・TASK-95・RLS-9）。
# `#[ignore]`・release 専用・手動専用。TENANT_LATENCY_ROUNDS（既定 200・200 未満は拒否）／
# TENANT_LATENCY_WARMUP（既定 20）で規模を上書きできる。1 プロセス = 1 計測
# （docs/design/benchmark-judgement-policy.md §5 準拠）。実行記録は PR 本文へ転記する。
set -euo pipefail

echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "sha: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
cargo test --release -p fandhe-db-wire-server --test wire_tenant_row_id_scope -- --ignored --nocapture --exact rls9_wire_insert_latency_distribution_is_indistinguishable_for_foreign_held_id_and_absent_id
